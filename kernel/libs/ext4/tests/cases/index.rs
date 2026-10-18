// SPDX-License-Identifier: MPL-2.0

use super::*;

fn long_name(number: u32) -> Vec<u8> {
    let mut name = format!("item{number:08}-").into_bytes();
    name.resize(255, b'z');
    name
}

fn work_inode(fs: &Ext4<Disk>) -> Inode {
    fs.lookup(&fs.inode(2).unwrap(), b"work").unwrap()
}

fn create_long_entry(
    fs: &mut WritableExt4<Disk>,
    parent: u32,
    number: u32,
) -> Result<Inode, Error> {
    fs.create_file(parent, &long_name(number), created_metadata(number + 2))
}

fn create_long_entries(fs: &mut WritableExt4<Disk>, parent: u32, range: std::ops::Range<u32>) {
    for number in range {
        create_long_entry(fs, parent, number).unwrap();
    }
}

fn start_directory(disk: &Disk) -> (WritableExt4<Disk>, u32) {
    let mut fs = disk.write_session();
    let parent = fs
        .create_directory(2, b"work", created_metadata(1))
        .unwrap()
        .number();
    (fs, parent)
}

fn remove_long_entries(
    fs: &mut WritableExt4<Disk>,
    parent: u32,
    numbers: impl IntoIterator<Item = u32>,
) {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for number in numbers {
        let inode = fs.unlink(parent, &long_name(number), changed).unwrap();
        fs.reap(inode.number()).unwrap();
    }
}

fn small_directory_fixture() -> Disk {
    let disk = smaller_block_fixture(1024);
    let (mut fs, parent) = start_directory(&disk);
    create_long_entries(&mut fs, parent, 0..3);
    fs.close().unwrap();
    disk
}

#[test]
fn grows_index_roots_and_splits_interior_nodes_without_losing_names() {
    let disk = small_directory_fixture();
    let mut fs = disk.write_session();
    let parent = work_inode(fs.reader().unwrap()).number();
    create_long_entries(&mut fs, parent, 3..600);
    let reader = fs.reader().unwrap();
    let inode = reader.inode(parent).unwrap();
    assert_ne!(inode.flags() & 0x1000, 0);
    let root_block = reader.file_block(&inode, 0).unwrap().unwrap();
    let raw = disk.read(root_block * 1024, 1024);
    assert_eq!(raw[30], 1);
    assert!(u16::from_le_bytes(raw[34..36].try_into().unwrap()) > 1);
    assert_eq!(reader.read_dir(&inode).unwrap().count(), 602);
    for number in 0..600 {
        assert_eq!(
            reader
                .lookup(&inode, &long_name(number))
                .unwrap()
                .generation(),
            number + 2
        );
    }
    fs.close().unwrap();
    export(&disk, "indexed-grown-1024-after.ext4");

    let mut fs = disk.write_session();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    remove_long_entries(&mut fs, parent, 0..600);
    let inode = fs.remove_directory(2, b"work", changed).unwrap();
    fs.reap(inode.number()).unwrap();
    fs.close().unwrap();
    export(&disk, "indexed-reclaimed-1024-after.ext4");
}

#[test]
fn mutates_and_moves_linux_indexed_directories() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/indexed.sparse"));
    let mut fs = disk.write_session();
    let reader = fs.reader().unwrap();
    let parent = reader
        .lookup(&reader.inode(2).unwrap(), b"indexed")
        .unwrap()
        .number();
    create_long_entries(&mut fs, parent, 0..100);
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fs.link(parent, b"linked", PAYLOAD_INODE, changed).unwrap();
    fs.rename(parent, b"name0537", 2, b"moved-file", changed)
        .unwrap();
    let removed = fs.unlink(parent, b"name0999", changed).unwrap();
    fs.reap(removed.number()).unwrap();
    let destination = fs
        .create_directory(2, b"destination", created_metadata(400))
        .unwrap();
    fs.rename(2, b"indexed", destination.number(), b"moved", changed)
        .unwrap();
    let reader = fs.reader().unwrap();
    let inode = reader.inode(parent).unwrap();
    assert_eq!(
        reader.lookup(&inode, b"..").unwrap().number(),
        destination.number()
    );
    assert_eq!(
        reader.lookup(&inode, b"linked").unwrap().number(),
        PAYLOAD_INODE
    );
    assert_eq!(reader.read_dir(&inode).unwrap().count(), 1101);
    for number in 0..100 {
        assert!(reader.lookup(&inode, &long_name(number)).is_ok());
    }
    fs.close().unwrap();
    export(&disk, "indexed-foreign-moved-4096-after.ext4");
}

#[test]
fn recovers_index_conversion_and_leaf_splits_at_each_io_failure() {
    fault_each_write(
        small_directory_fixture,
        |fs| {
            let parent = work_inode(fs.reader()?).number();
            fs.create_file(parent, &long_name(3), created_metadata(5))
                .map(|_| ())
        },
        |reader, _, cut, retained| {
            let parent = work_inode(reader);
            for number in 0..3 {
                assert!(reader.lookup(&parent, &long_name(number)).is_ok());
            }
            let added = match reader.lookup(&parent, &long_name(3)) {
                Ok(_) => 1,
                Err(Error::NotFound) => 0,
                Err(error) => panic!("cut {cut}, retained {retained}: {error:?}"),
            };
            assert_eq!(reader.read_dir(&parent).unwrap().count(), 5 + added);
        },
    );
}

#[test]
fn leaves_linux_replayable_index_conversion_commits() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let log = Log::new(mode, 1, 42);
        let mut fs = log.start_session();
        let parent = fs
            .create_directory(2, b"work", created_metadata(1))
            .unwrap();
        create_long_entries(&mut fs, parent.number(), 0..15);
        log.disk.0.borrow_mut().fail_offset = Some(0);
        assert!(matches!(
            fs.create_file(parent.number(), &long_name(15), created_metadata(17)),
            Err(Error::Io(IoError::Failed))
        ));
        drop(fs);
        log.disk.crash(0);
        export(
            &log.disk,
            &format!("indexed-conversion-{mode:?}-before.ext4"),
        );
        let fs = log.recover().unwrap();
        let parent = work_inode(&fs);
        assert_ne!(parent.flags() & 0x1000, 0);
        for number in 0..16 {
            assert!(fs.lookup(&parent, &long_name(number)).is_ok());
        }
    }
}

#[test]
fn follows_collision_continuations_and_retains_empty_indexed_leaves() {
    let disk = smaller_block_fixture(1024);
    let (mut fs, parent) = start_directory(&disk);
    // e2fsprogs hashes both 26935 and 51969 to 0xbdb92550 with this fixture's seed.
    for number in [0, 26935, 51969, 3] {
        fs.create_file(parent, &long_name(number), created_metadata(number))
            .unwrap();
    }
    let reader = fs.reader().unwrap();
    let inode = reader.inode(parent).unwrap();
    let root = disk.read(reader.file_block(&inode, 0).unwrap().unwrap() * 1024, 1024);
    assert_eq!(u32::from_le_bytes(root[40..44].try_into().unwrap()) & 1, 1);
    for number in [26935, 51969] {
        assert_eq!(
            reader
                .lookup(&inode, &long_name(number))
                .unwrap()
                .generation(),
            number
        );
    }
    remove_long_entries(&mut fs, parent, [0, 26935]);
    let reader = fs.reader().unwrap();
    let inode = reader.inode(parent).unwrap();
    assert!(reader.lookup(&inode, &long_name(51969)).is_ok());
    assert_eq!(reader.read_dir(&inode).unwrap().count(), 4);
    fs.close().unwrap();
    export(&disk, "indexed-collision-after.ext4");
}

#[test]
fn distinguishes_empty_leaves_from_index_nodes_without_metadata_checksums() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/rw-1024-nocsum.sparse"));
    let (mut fs, parent) = start_directory(&disk);
    create_long_entries(&mut fs, parent, 0..8);
    remove_long_entries(&mut fs, parent, 0..8);
    let reader = fs.reader().unwrap();
    let inode = reader.inode(parent).unwrap();
    assert_ne!(inode.flags() & 0x1000, 0);
    assert_eq!(
        reader
            .read_dir(&inode)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        reader.lookup(&inode, b"absent"),
        Err(Error::NotFound)
    ));
    fs.close().unwrap();
    export(&disk, "indexed-empty-nocsum-after.ext4");
}

fn growth_snapshots() -> Vec<(Disk, u32, &'static str)> {
    let disk = small_directory_fixture();
    let mut fs = disk.write_session();
    let parent = work_inode(fs.reader().unwrap()).number();
    let mut snapshots = Vec::new();
    let mut previous = (0, 0);
    for number in 3..600 {
        let before = disk.snapshot();
        create_long_entry(&mut fs, parent, number).unwrap();
        let reader = fs.reader().unwrap();
        let inode = reader.inode(parent).unwrap();
        let block = reader.file_block(&inode, 0).unwrap().unwrap();
        let raw = disk.read(block * 1024, 1024);
        let current = (raw[30], u16::from_le_bytes(raw[34..36].try_into().unwrap()));
        if current.0 == 1 && previous.0 == 0 {
            snapshots.push((before, number, "root-growth"));
        } else if previous.0 == 1 && current.1 > previous.1 {
            snapshots.push((before, number, "interior-split"));
            break;
        }
        previous = current;
    }
    assert_eq!(snapshots.len(), 2);
    fs.close().unwrap();
    snapshots
}

fn assert_growing_directory(reader: &Ext4<Disk>, count: u32) {
    let inode = work_inode(reader);
    for number in [0, count / 2, count - 1] {
        assert!(reader.lookup(&inode, &long_name(number)).is_ok());
    }
    let added = match reader.lookup(&inode, &long_name(count)) {
        Ok(_) => 1,
        Err(Error::NotFound) => 0,
        Err(error) => panic!("lookup after recovery: {error:?}"),
    };
    let mut names = reader
        .read_dir(&inode)
        .unwrap()
        .map(|entry| entry.unwrap().name().to_vec())
        .collect::<Vec<_>>();
    names.sort();
    let mut expected = vec![b".".to_vec(), b"..".to_vec()];
    expected.extend((0..count + added).map(long_name));
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn recovers_index_root_growth_and_interior_splits_at_each_io_failure() {
    for (snapshot, number, stage) in growth_snapshots() {
        fault_each_write(
            || snapshot.snapshot(),
            |fs| {
                let parent = work_inode(fs.reader()?).number();
                fs.create_file(parent, &long_name(number), created_metadata(number + 2))
                    .map(|_| ())
            },
            |reader, _, _, _| assert_growing_directory(reader, number),
        );
        let disk = snapshot.snapshot();
        let mut fs = disk.write_session();
        let parent = work_inode(fs.reader().unwrap()).number();
        disk.0.borrow_mut().fail_offset = Some(1024);
        assert!(matches!(
            fs.create_file(parent, &long_name(number), created_metadata(number + 2)),
            Err(Error::Io(IoError::Failed))
        ));
        drop(fs);
        disk.crash(0);
        export(
            &disk,
            &format!("indexed-{stage}-{}-before.ext4", number + 1),
        );
        let reader = Ext4::recover(disk, limits()).unwrap();
        assert_growing_directory(&reader, number);
        assert!(
            reader
                .lookup(&work_inode(&reader), &long_name(number))
                .is_ok()
        );
    }
}

fn store_index_block(disk: &Disk, fs: &Ext4<Disk>, inode: &Inode, logical: u32, mut raw: Vec<u8>) {
    let offset = if logical == 0 { 32 } else { 8 };
    let count = usize::from(u16::from_le_bytes(
        raw[offset + 2..offset + 4].try_into().unwrap(),
    ));
    let limit = usize::from(u16::from_le_bytes(
        raw[offset..offset + 2].try_into().unwrap(),
    ));
    let tail = offset + limit * 8;
    let seed = checksum::crc32c(fs.geometry().checksum_seed(), &inode.number().to_le_bytes());
    let seed = checksum::crc32c(seed, &inode.generation().to_le_bytes());
    let crc = checksum::crc32c(seed, &raw[..offset + count * 8]);
    let crc = checksum::crc32c(crc, &[0; 8]);
    raw[tail..tail + 8].fill(0);
    raw[tail + 4..tail + 8].copy_from_slice(&crc.to_le_bytes());
    let physical = fs.file_block(inode, logical).unwrap().unwrap();
    disk.set(physical * u64::from(fs.geometry().block_size()), &raw);
}

#[test]
fn rejects_index_nodes_referenced_as_leaves_even_with_valid_checksums() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/indexed.sparse"));
    let fs = Ext4::open(disk.clone()).unwrap();
    let inode = fs.lookup(&fs.inode(2).unwrap(), b"indexed").unwrap();
    let root_block = fs.file_block(&inode, 0).unwrap().unwrap();
    let mut root = disk.read(root_block * BLOCK_SIZE as u64, BLOCK_SIZE);
    assert!(u16::from_le_bytes(root[34..36].try_into().unwrap()) >= 3);
    let children =
        [36, 44, 52].map(|offset| u32::from_le_bytes(root[offset..offset + 4].try_into().unwrap()));
    let name = fs
        .read_dir_from(&inode, u64::from(children[0]) * BLOCK_SIZE as u64)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .name()
        .to_vec();
    root[30] = 1;
    store_index_block(&disk, &fs, &inode, 0, root);
    for (node, child) in [(children[0], children[1]), (children[1], children[2])] {
        let mut raw = vec![0; BLOCK_SIZE];
        raw[4..6].copy_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());
        raw[8..10].copy_from_slice(&(((BLOCK_SIZE - 16) / 8) as u16).to_le_bytes());
        raw[10..12].copy_from_slice(&1u16.to_le_bytes());
        raw[12..16].copy_from_slice(&child.to_le_bytes());
        store_index_block(&disk, &fs, &inode, node, raw);
    }
    export(&disk, "indexed-node-as-leaf-corrupt.ext4");
    assert_eq!(
        fs.lookup(&inode, &name).map(|inode| inode.number()),
        Err(Error::Corrupt)
    );
    let mut fs = disk.write_session();
    disk.0.borrow_mut().operations = 0;
    assert!(matches!(
        fs.create_file(inode.number(), &name, created_metadata(1)),
        Err(Error::Corrupt)
    ));
    assert_eq!(disk.0.borrow().operations, 0);
}
