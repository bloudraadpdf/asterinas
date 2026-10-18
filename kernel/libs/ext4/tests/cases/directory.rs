// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn reads_checked_linear_and_indexed_directories() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/indexed.sparse"));
    let fs = Ext4::open(disk).unwrap();
    let root = fs.inode(2).unwrap();
    assert_eq!(
        fs.lookup(&root, b"payload").unwrap().number(),
        PAYLOAD_INODE
    );
    let directory = fs.lookup(&root, b"indexed").unwrap();
    let entries = fs
        .read_dir(&directory)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1002);
    let first = &entries[0];
    assert_eq!(first.name(), b".");
    assert_eq!(first.inode(), directory.number());
    assert_eq!(first.file_type(), 2);
    assert_eq!(
        fs.read_dir_from(&directory, first.next_offset())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .name(),
        b".."
    );
    let mut names: Vec<_> = entries.iter().map(|entry| entry.name().to_vec()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 1002);
    for name in [b"name0000", b"name0537", b"name0999"] {
        assert_eq!(fs.lookup(&directory, name).unwrap().size(), 0);
    }
    assert!(matches!(
        fs.lookup(&directory, b"absent"),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        fs.lookup(&directory, b"invalid/name"),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        fs.lookup(&directory, &vec![b'x'; 256]),
        Err(Error::NameTooLong)
    ));
}

#[test]
fn rejects_directory_index_and_leaf_corruption() {
    for logical in [0, 1] {
        let disk = Disk::from_sparse(include_bytes!("../fixtures/indexed.sparse"));
        let fs = Ext4::open(disk.clone()).unwrap();
        let root = fs.inode(2).unwrap();
        let directory = fs.lookup(&root, b"indexed").unwrap();
        let physical = fs.file_block(&directory, logical).unwrap().unwrap();
        let offset = physical * BLOCK_SIZE as u64 + (BLOCK_SIZE - 4) as u64;
        let byte = disk.read(offset, 1)[0];
        disk.set(offset, &[byte ^ 1]);
        assert!(fs.read_dir(&directory).unwrap().any(|entry| entry.is_err()));
    }
}

#[test]
fn creates_files_and_directories_with_checked_allocation_and_metadata() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let free = fs.geometry().free_inodes();
    let directory = fs
        .create_directory(2, b"work", created_metadata(1))
        .unwrap();
    assert_eq!(directory.mode(), 0o40750);
    assert_eq!(directory.links(), 2);
    assert_eq!((directory.uid(), directory.gid()), (1000, 1001));
    let mut expected_names = vec![b".".to_vec(), b"..".to_vec()];
    for index in 0..1000 {
        let name = format!("file{index:04}").into_bytes();
        let inode = fs
            .create_file(directory.number(), &name, created_metadata(index + 2))
            .unwrap();
        assert_eq!(inode.mode(), 0o100750);
        assert_eq!(inode.size(), 0);
        assert_eq!(inode.links(), 1);
        assert_eq!((inode.uid(), inode.gid()), (1000, 1001));
        if index == 0 {
            assert_eq!(
                fs.write_at(
                    inode.number(),
                    4096,
                    b"created-data",
                    created_metadata(0).created
                )
                .unwrap(),
                12
            );
        }
        expected_names.push(name);
    }
    let directory = fs.reader().unwrap().inode(directory.number()).unwrap();
    let mut actual_names = fs
        .reader()
        .unwrap()
        .read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().name().to_vec())
        .collect::<Vec<_>>();
    expected_names.sort();
    actual_names.sort();
    assert_eq!(actual_names, expected_names);
    assert_eq!(fs.geometry().free_inodes(), free - 1001);
    assert_eq!(
        fs.reader()
            .unwrap()
            .lookup(&directory, b"..")
            .unwrap()
            .number(),
        2
    );
    assert_eq!(fs.reader().unwrap().inode(2).unwrap().links(), 4);
    assert!(matches!(
        fs.create_file(directory.number(), b"file0000", created_metadata(2000)),
        Err(Error::AlreadyExists)
    ));
    assert!(matches!(
        fs.create_directory(2, b".", created_metadata(2000)),
        Err(Error::InvalidInput)
    ));
    fs.close().unwrap();
    export(&disk, "created-after.ext4");
}

#[test]
fn recovers_atomic_directory_creation_after_each_write_and_flush_failure() {
    fault_each_write(
        Disk::fixture,
        |fs| {
            fs.create_directory(2, b"work", created_metadata(1))
                .map(|_| ())
        },
        |fs, _, cut, retained| {
            let root = fs.inode(2).unwrap();
            match fs.lookup(&root, b"work") {
                Err(Error::NotFound) => assert_eq!(root.links(), 3),
                Ok(_) => assert_work_directory(fs),
                Err(error) => panic!("cut {cut}, retained {retained}: {error:?}"),
            }
        },
    );
}

#[test]
fn links_and_relinks_live_inodes_atomically() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
    let linked = fs.link(2, b"alias", PAYLOAD_INODE, changed).unwrap();
    assert_eq!(linked.links(), 2);
    fs.unlink(2, b"payload", changed).unwrap();
    assert_eq!(
        fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap().links(),
        1
    );
    fs.unlink(2, b"alias", changed).unwrap();
    fs.link(2, b"relinked", PAYLOAD_INODE, changed).unwrap();
    assert_eq!(
        fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap().links(),
        1
    );
    assert_eq!(
        (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
        free
    );
    fs.close().unwrap();
    export(&disk, "relinked-after.ext4");
}

#[test]
fn recovers_hard_links_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        Disk::fixture,
        |fs| fs.link(2, b"alias", PAYLOAD_INODE, changed).map(|_| ()),
        |fs, _, _, _| {
            let root = fs.inode(2).unwrap();
            let inode = fs.lookup(&root, b"payload").unwrap();
            match fs.lookup(&root, b"alias") {
                Ok(alias) => {
                    assert_eq!(alias.number(), PAYLOAD_INODE);
                    assert_eq!(inode.links(), 2);
                }
                Err(Error::NotFound) => assert_eq!(inode.links(), 1),
                Err(error) => panic!("unexpected lookup error: {error:?}"),
            }
        },
    );
}

#[test]
fn removes_directories_without_leaking_blocks_inodes_or_links() {
    let (disk, mut fs) = writable_fixture();
    let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let parent = fs
        .create_directory(2, b"work", created_metadata(1))
        .unwrap();
    let child = fs
        .create_file(parent.number(), b"child", created_metadata(2))
        .unwrap();
    assert!(matches!(
        fs.unlink(2, b"work", changed),
        Err(Error::IsDirectory)
    ));
    assert!(matches!(
        fs.remove_directory(2, b"work", changed),
        Err(Error::NotEmpty)
    ));
    fs.unlink(parent.number(), b"child", changed).unwrap();
    fs.reap(child.number()).unwrap();
    fs.remove_directory(2, b"work", changed).unwrap();
    fs.reap(parent.number()).unwrap();
    assert_eq!(fs.reader().unwrap().inode(2).unwrap().links(), 3);
    assert_eq!(
        (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
        free
    );
    fs.close().unwrap();
    export(&disk, "removed-directory-after.ext4");
}

#[test]
fn recovers_directory_removal_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        empty_directory_fixture,
        |fs| {
            let inode = fs.remove_directory(2, b"work", changed)?;
            fs.reap(inode.number())
        },
        |fs, _, _, _| {
            let root = fs.inode(2).unwrap();
            match fs.lookup(&root, b"work") {
                Ok(inode) => {
                    assert_eq!(root.links(), 4);
                    assert_eq!(inode.links(), 2);
                }
                Err(Error::NotFound) => assert_eq!(root.links(), 3),
                Err(error) => panic!("unexpected lookup error: {error:?}"),
            }
        },
    );
}

#[test]
fn renames_files_and_directories_with_replacement_and_cycle_checks() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let (left, right) = create_parent_directories(&mut fs);
    fs.rename(2, b"payload", left.number(), b"source", changed)
        .unwrap();
    let victim = fs
        .create_file(right.number(), b"victim", created_metadata(3))
        .unwrap();
    let replaced = fs
        .rename(left.number(), b"source", right.number(), b"victim", changed)
        .unwrap()
        .unwrap();
    assert_eq!(replaced.number(), victim.number());
    assert_eq!(replaced.links(), 0);
    fs.reap(victim.number()).unwrap();
    let child = fs
        .create_directory(left.number(), b"child", created_metadata(4))
        .unwrap();
    let empty = fs
        .create_directory(right.number(), b"empty", created_metadata(5))
        .unwrap();
    let replaced = fs
        .rename(left.number(), b"child", right.number(), b"empty", changed)
        .unwrap()
        .unwrap();
    assert_eq!(replaced.number(), empty.number());
    fs.reap(empty.number()).unwrap();
    let reader = fs.reader().unwrap();
    let child = reader.inode(child.number()).unwrap();
    assert_eq!(
        reader.lookup(&child, b"..").unwrap().number(),
        right.number()
    );
    assert_eq!(reader.inode(left.number()).unwrap().links(), 2);
    assert_eq!(reader.inode(right.number()).unwrap().links(), 3);
    assert!(matches!(
        fs.rename(2, b"right", child.number(), b"cycle", changed),
        Err(Error::InvalidInput)
    ));
    fs.link(right.number(), b"alias", PAYLOAD_INODE, changed)
        .unwrap();
    assert!(
        fs.rename(right.number(), b"victim", right.number(), b"alias", changed)
            .unwrap()
            .is_none()
    );
    assert!(
        fs.rename(
            right.number(),
            b"victim",
            right.number(),
            b"victim",
            changed
        )
        .unwrap()
        .is_none()
    );
    fs.close().unwrap();
    export(&disk, "renamed-after.ext4");
}

#[test]
fn recovers_rename_replacement_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        rename_fixture,
        |fs| {
            if let Some(inode) = fs.rename(2, b"payload", 2, b"victim", changed)? {
                fs.reap(inode.number())?;
            }
            Ok(())
        },
        |fs, _, _, _| {
            let root = fs.inode(2).unwrap();
            let target = fs.lookup(&root, b"victim").unwrap();
            match fs.lookup(&root, b"payload") {
                Ok(inode) => {
                    assert_eq!(inode.number(), PAYLOAD_INODE);
                    assert_ne!(target.number(), PAYLOAD_INODE);
                }
                Err(Error::NotFound) => {
                    assert_eq!(target.number(), PAYLOAD_INODE);
                    assert_eq!(target.links(), 1);
                }
                Err(error) => panic!("unexpected lookup error: {error:?}"),
            }
        },
    );
}

#[test]
fn recovers_directory_moves_and_parent_links_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        directory_move_fixture,
        |fs| {
            let reader = fs.reader()?;
            let root = reader.inode(2)?;
            let left = reader.lookup(&root, b"left")?.number();
            let right = reader.lookup(&root, b"right")?.number();
            let replaced = fs
                .rename(left, b"source", right, b"victim", changed)?
                .unwrap();
            fs.reap(replaced.number())
        },
        |fs, _, _, _| {
            let root = fs.inode(2).unwrap();
            let left = fs.lookup(&root, b"left").unwrap();
            let right = fs.lookup(&root, b"right").unwrap();
            let target = fs.lookup(&right, b"victim").unwrap();
            assert_eq!(right.links(), 3);
            assert_eq!(fs.lookup(&target, b"..").unwrap().number(), right.number());
            match fs.lookup(&left, b"source") {
                Ok(source) => {
                    assert_eq!(left.links(), 3);
                    assert_eq!(fs.lookup(&source, b"..").unwrap().number(), left.number());
                    assert_ne!(source.number(), target.number());
                }
                Err(Error::NotFound) => assert_eq!(left.links(), 2),
                Err(error) => panic!("unexpected lookup error: {error:?}"),
            }
        },
    );
}

#[test]
fn creates_reads_links_and_reclaims_inline_and_block_symlinks() {
    for length in [1, 59, 60, 4095] {
        let (disk, mut fs) = writable_fixture();
        let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
        let changed = Timestamp::new(1_600_000_001, 0).unwrap();
        let target = vec![b'a'; length];
        let inode = fs
            .create_symlink(2, b"link", &target, created_metadata(1))
            .unwrap();
        assert_eq!(fs.reader().unwrap().read_link(&inode).unwrap(), target);
        fs.link(2, b"alias", inode.number(), changed).unwrap();
        fs.rename(2, b"link", 2, b"moved", changed).unwrap();
        assert_eq!(
            fs.reader().unwrap().inode(inode.number()).unwrap().links(),
            2
        );
        fs.close().unwrap();
        export(&disk, &format!("symlink-{length}-after.ext4"));
        let mut fs = disk.write_session();
        fs.unlink(2, b"moved", changed).unwrap();
        fs.unlink(2, b"alias", changed).unwrap();
        fs.reap(inode.number()).unwrap();
        assert_free_counts(&fs, free);
        fs.close().unwrap();
        export(&disk, &format!("symlink-{length}-removed-after.ext4"));
    }
}

#[test]
fn recovers_symlink_creation_and_removal_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for length in [59, 4095] {
        let target = vec![b'a'; length];
        fault_each_write(
            Disk::fixture,
            |fs| {
                fs.create_symlink(2, b"link", &target, created_metadata(1))
                    .map(|_| ())
            },
            |fs, _, _, _| assert_optional_symlink(fs, &target),
        );
        fault_each_write(
            || symlink_fixture(&target),
            |fs| {
                let inode = fs.unlink(2, b"link", changed)?;
                fs.reap(inode.number())
            },
            |fs, _, _, _| assert_optional_symlink(fs, &target),
        );
    }
}

#[test]
fn rejects_namespace_mutation_in_removed_directories() {
    let (disk, mut fs) = writable_fixture();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let parent = fs
        .create_directory(2, b"removed", created_metadata(1))
        .unwrap();
    fs.remove_directory(2, b"removed", changed).unwrap();
    disk.0.borrow_mut().operations = 0;
    assert!(matches!(
        fs.create_file(parent.number(), b"child", created_metadata(2)),
        Err(Error::NotFound)
    ));
    assert_eq!(disk.0.borrow().operations, 0);
    fs.reap(parent.number()).unwrap();
    fs.close().unwrap();
}
