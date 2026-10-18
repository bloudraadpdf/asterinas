// SPDX-License-Identifier: MPL-2.0

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::Path,
    rc::Rc,
};

use aster_ext4::{
    AttributeMode, CreateMetadata, DeviceNumber, Error, Ext4, Inode, MetadataUpdate, MountLimits,
    SpecialFile, Timestamp, WritableExt4,
};
use io_util::{IoError, ReadAt, WriteAt, checksum};

const BLOCK_SIZE: usize = 4096;
const MAGIC: u32 = 0xc03b_3998;
const PAYLOAD_INODE: u32 = 13;

type Blocks = BTreeMap<u64, Vec<u8>>;

#[derive(Clone)]
struct Disk(Rc<RefCell<State>>);

#[derive(Clone)]
struct State {
    size: u64,
    stable: Blocks,
    pending: Blocks,
    operations: usize,
    fail_at: Option<usize>,
    fail_offset: Option<u64>,
    read_only: bool,
}

impl Disk {
    fn fixture() -> Self {
        Self::from_sparse(include_bytes!("fixtures/journal.sparse"))
    }

    fn write_session(&self) -> WritableExt4<Self> {
        WritableExt4::open(self.clone(), limits()).unwrap()
    }

    fn orphan_fixture(orphan_file: bool) -> Self {
        if orphan_file {
            Self::fixture()
        } else {
            Self::from_sparse(include_bytes!("fixtures/classic.sparse"))
        }
    }

    fn from_sparse(raw: &[u8]) -> Self {
        assert_eq!(&raw[..8], b"EXT4SP01");
        let size = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let count = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        let mut cursor = 20;
        let mut stable = BTreeMap::new();
        for _ in 0..count {
            let offset = u64::from_le_bytes(raw[cursor..cursor + 8].try_into().unwrap());
            let length =
                u32::from_le_bytes(raw[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
            cursor += 12;
            assert_eq!(length, BLOCK_SIZE);
            assert!(offset.is_multiple_of(BLOCK_SIZE as u64));
            assert!(offset + length as u64 <= size);
            assert!(
                stable
                    .insert(
                        offset / BLOCK_SIZE as u64,
                        raw[cursor..cursor + length].to_vec()
                    )
                    .is_none()
            );
            cursor += length;
        }
        assert_eq!(cursor, raw.len());
        Self(Rc::new(RefCell::new(State {
            size,
            stable,
            pending: BTreeMap::new(),
            operations: 0,
            fail_at: None,
            fail_offset: None,
            read_only: false,
        })))
    }

    fn read(&self, offset: u64, length: usize) -> Vec<u8> {
        let mut data = vec![0; length];
        self.read_exact_at(offset, &mut data).unwrap();
        data
    }

    fn set(&self, offset: u64, raw: &[u8]) {
        self.write_all_at(offset, raw).unwrap();
        self.flush().unwrap();
        self.0.borrow_mut().operations = 0;
    }

    fn mutate_inode(&self, number: u32, change: impl FnOnce(&mut [u8])) {
        let fs = Ext4::open(self.clone()).unwrap();
        let geometry = fs.geometry();
        let group = fs
            .group((number - 1) / geometry.inodes_per_group())
            .unwrap();
        let offset = group.inode_table() * u64::from(geometry.block_size())
            + u64::from((number - 1) % geometry.inodes_per_group())
                * u64::from(geometry.inode_size());
        let mut raw = self.read(offset, geometry.inode_size() as usize);
        change(&mut raw);
        raw[124..126].fill(0);
        raw[130..132].fill(0);
        let seed = checksum::crc32c(geometry.checksum_seed(), &number.to_le_bytes());
        let seed = checksum::crc32c(seed, &raw[100..104]);
        let crc = checksum::crc32c(seed, &raw);
        raw[124..126].copy_from_slice(&(crc as u16).to_le_bytes());
        raw[130..132].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
        self.set(offset, &raw);
    }

    fn snapshot(&self) -> Self {
        Self(Rc::new(RefCell::new(self.0.borrow().clone())))
    }

    fn fault(&self, operation: usize) {
        let mut state = self.0.borrow_mut();
        state.operations = 0;
        state.fail_at = Some(operation);
    }

    fn crash(&self, retained: usize) {
        let mut state = self.0.borrow_mut();
        for (block, data) in std::mem::take(&mut state.pending) {
            if retained == 2 || (retained == 1 && block % 2 == 0) {
                state.stable.insert(block, data);
            }
        }
        state.fail_at = None;
        state.fail_offset = None;
        state.operations = 0;
    }

    fn export(&self, path: &Path) {
        let state = self.0.borrow();
        assert!(state.pending.is_empty());
        let mut file = File::create_new(path).unwrap();
        file.set_len(state.size).unwrap();
        for (block, data) in &state.stable {
            file.seek(SeekFrom::Start(block * BLOCK_SIZE as u64))
                .unwrap();
            file.write_all(data).unwrap();
        }
        file.sync_all().unwrap();
    }
}

impl State {
    fn operation(&mut self) -> Result<(), IoError> {
        let operation = self.operations;
        self.operations += 1;
        if self.fail_at == Some(operation) {
            return Err(IoError::Failed);
        }
        Ok(())
    }
}

impl ReadAt for Disk {
    fn len(&self) -> u64 {
        self.0.borrow().size
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), IoError> {
        let state = self.0.borrow();
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > state.size)
        {
            return Err(IoError::Failed);
        }
        let mut copied = 0;
        while copied < output.len() {
            let position = offset + copied as u64;
            let block = position / BLOCK_SIZE as u64;
            let within = position as usize % BLOCK_SIZE;
            let length = (BLOCK_SIZE - within).min(output.len() - copied);
            if let Some(raw) = state
                .pending
                .get(&block)
                .or_else(|| state.stable.get(&block))
            {
                output[copied..copied + length].copy_from_slice(&raw[within..within + length]);
            } else {
                output[copied..copied + length].fill(0);
            }
            copied += length;
        }
        Ok(())
    }
}

impl WriteAt for Disk {
    fn is_read_only(&self) -> bool {
        self.0.borrow().read_only
    }

    fn write_all_at(&self, offset: u64, input: &[u8]) -> Result<(), IoError> {
        let mut state = self.0.borrow_mut();
        if state.read_only
            || offset
                .checked_add(input.len() as u64)
                .is_none_or(|end| end > state.size)
        {
            return Err(IoError::Failed);
        }
        state.operation()?;
        if state.fail_offset == Some(offset) {
            return Err(IoError::Failed);
        }
        let first = offset / BLOCK_SIZE as u64;
        let end = (offset + input.len() as u64).div_ceil(BLOCK_SIZE as u64);
        for block in first..end {
            let mut data = state
                .pending
                .get(&block)
                .or_else(|| state.stable.get(&block))
                .cloned()
                .unwrap_or_else(|| vec![0; BLOCK_SIZE]);
            let start = offset.max(block * BLOCK_SIZE as u64);
            let end = (offset + input.len() as u64).min((block + 1) * BLOCK_SIZE as u64);
            data[(start - block * BLOCK_SIZE as u64) as usize
                ..(end - block * BLOCK_SIZE as u64) as usize]
                .copy_from_slice(&input[(start - offset) as usize..(end - offset) as usize]);
            state.pending.insert(block, data);
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), IoError> {
        let mut state = self.0.borrow_mut();
        state.operation()?;
        for (block, data) in std::mem::take(&mut state.pending) {
            state.stable.insert(block, data);
        }
        Ok(())
    }
}

fn limits() -> MountLimits {
    MountLimits {
        journal_blocks: 4096,
        transactions: 16,
        revocations: 64,
        mapping_entries: 64,
        orphan_inodes: 64,
        metadata_blocks: 16,
        data_blocks: 4,
        allocation_groups: 64,
        directory_depth: 128,
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    None,
    V1,
    V2,
    V3,
}

impl Mode {
    fn has_block_checksums(self) -> bool {
        matches!(self, Self::V2 | Self::V3)
    }
}

struct Log {
    disk: Disk,
    mode: Mode,
    uuid_seed: u32,
    mapping: Vec<u64>,
    sequence: u32,
    cursor: u32,
    target: u64,
    last_descriptor: u32,
    last_data: u32,
    last_commit: u32,
}

impl Log {
    fn new(mode: Mode, start: u32, sequence: u32) -> Self {
        let disk = Disk::fixture();
        let fs = Ext4::open(disk.clone()).unwrap();
        let inode = fs.inode(fs.geometry().journal_inode()).unwrap();
        let mapping = (0..1024)
            .map(|logical| fs.file_block(&inode, logical).unwrap().unwrap())
            .collect::<Vec<_>>();
        let target = fs
            .file_block(&fs.inode(PAYLOAD_INODE).unwrap(), 0)
            .unwrap()
            .unwrap();
        let mut raw = disk.read(mapping[0] * BLOCK_SIZE as u64, 1024);
        raw[24..28].copy_from_slice(&sequence.to_be_bytes());
        raw[28..32].copy_from_slice(&start.to_be_bytes());
        raw[36..40].copy_from_slice(&u32::from(matches!(mode, Mode::V1)).to_be_bytes());
        let incompatible: u32 = 1
            | 2
            | match mode {
                Mode::V2 => 8,
                Mode::V3 => 16,
                _ => 0,
            };
        raw[40..44].copy_from_slice(&incompatible.to_be_bytes());
        raw[80] = if mode.has_block_checksums() { 4 } else { 0 };
        raw[252..256].fill(0);
        let uuid_seed = checksum::crc32c(!0, &raw[48..64]);
        if mode.has_block_checksums() {
            let crc = checksum::crc32c(!0, &raw);
            raw[252..256].copy_from_slice(&crc.to_be_bytes());
        }
        disk.set(mapping[0] * BLOCK_SIZE as u64, &raw);
        let mut superblock = disk.read(1024, 1024);
        superblock[0x60] |= 4;
        superblock[0x66] |= 1;
        set_super_checksum(&mut superblock);
        disk.set(1024, &superblock);
        Self {
            disk,
            mode,
            uuid_seed,
            mapping,
            sequence,
            cursor: start,
            target,
            last_descriptor: 0,
            last_data: 0,
            last_commit: 0,
        }
    }

    fn header(&self, kind: u32) -> Vec<u8> {
        let mut raw = vec![0; BLOCK_SIZE];
        raw[..4].copy_from_slice(&MAGIC.to_be_bytes());
        raw[4..8].copy_from_slice(&kind.to_be_bytes());
        raw[8..12].copy_from_slice(&self.sequence.to_be_bytes());
        raw
    }

    fn metadata_checksum(&self, raw: &mut [u8]) {
        if self.mode.has_block_checksums() {
            raw[BLOCK_SIZE - 4..].fill(0);
            let crc = checksum::crc32c(self.uuid_seed, raw);
            raw[BLOCK_SIZE - 4..].copy_from_slice(&crc.to_be_bytes());
        }
    }

    fn append(&mut self, raw: &[u8]) -> u32 {
        let logical = self.cursor;
        self.disk
            .set(self.mapping[logical as usize] * BLOCK_SIZE as u64, raw);
        self.cursor = if logical == 1023 { 1 } else { logical + 1 };
        logical
    }

    fn commit(&mut self, transaction_crc: u32) {
        let mut raw = self.header(2);
        match self.mode {
            Mode::None => (),
            Mode::V1 => {
                raw[12] = 1;
                raw[13] = 4;
                raw[16..20].copy_from_slice(&transaction_crc.to_be_bytes());
            }
            Mode::V2 | Mode::V3 => {
                let crc = checksum::crc32c(self.uuid_seed, &raw);
                raw[16..20].copy_from_slice(&crc.to_be_bytes());
            }
        }
        self.last_commit = self.append(&raw);
        self.sequence = self.sequence.wrapping_add(1);
    }

    fn data(&mut self, target: u64, data: &[u8]) {
        assert_eq!(data.len(), BLOCK_SIZE);
        let escaped = data[..4] == MAGIC.to_be_bytes();
        let mut wire = data.to_vec();
        if escaped {
            wire[..4].fill(0);
        }
        let mut descriptor = self.header(1);
        descriptor[12..16].copy_from_slice(&(target as u32).to_be_bytes());
        let flags = 2 | 8 | u32::from(escaped);
        let crc = checksum::crc32c(self.uuid_seed, &self.sequence.to_be_bytes());
        let crc = checksum::crc32c(crc, &wire);
        if matches!(self.mode, Mode::V3) {
            descriptor[16..20].copy_from_slice(&flags.to_be_bytes());
            descriptor[20..24].copy_from_slice(&((target >> 32) as u32).to_be_bytes());
            descriptor[24..28].copy_from_slice(&crc.to_be_bytes());
        } else {
            descriptor[16..18].copy_from_slice(&(crc as u16).to_be_bytes());
            descriptor[18..20].copy_from_slice(&(flags as u16).to_be_bytes());
            descriptor[20..24].copy_from_slice(&((target >> 32) as u32).to_be_bytes());
        }
        self.metadata_checksum(&mut descriptor);
        let crc = checksum::crc32_be(checksum::crc32_be(!0, &descriptor), &wire);
        self.last_descriptor = self.append(&descriptor);
        self.last_data = self.append(&wire);
        self.commit(crc);
    }

    fn revoke(&mut self, target: u64) {
        let mut raw = self.header(5);
        raw[12..16].copy_from_slice(&24u32.to_be_bytes());
        raw[16..24].copy_from_slice(&target.to_be_bytes());
        self.metadata_checksum(&mut raw);
        self.append(&raw);
        self.commit(!0);
    }

    fn start_session(&self) -> WritableExt4<Disk> {
        self.recover().unwrap();
        WritableExt4::open(self.disk.clone(), limits()).unwrap()
    }

    fn overwrite_payload(&mut self, value: u8) -> Vec<u8> {
        let original = self.target_bytes();
        self.data(self.target, &vec![value; BLOCK_SIZE]);
        original
    }

    fn corrupt(&self, logical: u32, within: u64) {
        let offset = self.mapping[logical as usize] * BLOCK_SIZE as u64 + within;
        let byte = self.disk.read(offset, 1)[0];
        self.disk.set(offset, &[byte ^ 1]);
    }

    fn recover(&self) -> Result<Ext4<Disk>, Error> {
        Ext4::recover(self.disk.clone(), limits())
    }

    fn target_bytes(&self) -> Vec<u8> {
        self.disk.read(self.target * BLOCK_SIZE as u64, BLOCK_SIZE)
    }
}

fn set_super_checksum(raw: &mut [u8]) {
    let crc = checksum::crc32c(!0, &raw[..1020]);
    raw[1020..1024].copy_from_slice(&crc.to_le_bytes());
}

fn export(disk: &Disk, name: &str) {
    if let Some(root) = std::env::var_os("ASTER_EXT4_RECOVERY_OUTPUT") {
        disk.export(&Path::new(&root).join(name));
    }
}

fn fault_each_write(
    make_disk: impl Fn() -> Disk,
    operation_fn: impl Fn(&mut WritableExt4<Disk>) -> Result<(), Error>,
    check_fn: impl Fn(&Ext4<Disk>, &Disk, usize, usize),
) {
    let reference = make_disk();
    let mut fs = WritableExt4::open(reference.clone(), limits()).unwrap();
    reference.0.borrow_mut().operations = 0;
    operation_fn(&mut fs).unwrap();
    let operations = reference.0.borrow().operations;
    assert!(operations > 0);
    for cut in 0..operations {
        for retained in 0..3 {
            let disk = make_disk();
            let mut fs = disk.write_session();
            disk.fault(cut);
            assert_eq!(operation_fn(&mut fs), Err(Error::Io(IoError::Failed)));
            assert_eq!(fs.sync(), Err(Error::Aborted));
            assert!(matches!(fs.reader(), Err(Error::Aborted)));
            assert_eq!(
                fs.set_metadata(
                    PAYLOAD_INODE,
                    owner_change(1, 2, Timestamp::new(0, 0).unwrap())
                ),
                Err(Error::Aborted)
            );
            drop(fs);
            disk.crash(retained);
            let fs = Ext4::recover(disk.clone(), limits()).unwrap_or_else(|error| {
                panic!("recovery at cut {cut}, retained {retained}: {error:?}")
            });
            check_fn(&fs, &disk, cut, retained);
        }
    }
}

fn write_payload(fs: &mut WritableExt4<Disk>, offset: u64, data: &[u8]) {
    let modified = Timestamp::new(1_600_000_001, 0).unwrap();
    assert_eq!(
        fs.write_at(PAYLOAD_INODE, offset, data, modified).unwrap(),
        data.len()
    );
}

fn created_metadata(generation: u32) -> CreateMetadata {
    CreateMetadata {
        permissions: 0o750,
        uid: 1000,
        gid: 1001,
        created: Timestamp::new(1_600_000_001, 123).unwrap(),
        generation,
    }
}

fn assert_work_directory(fs: &Ext4<Disk>) {
    let root = fs.inode(2).unwrap();
    let directory = fs.lookup(&root, b"work").unwrap();
    assert_eq!(root.links(), 4);
    assert_eq!(directory.links(), 2);
    assert_eq!(
        fs.lookup(&directory, b".").unwrap().number(),
        directory.number()
    );
    assert_eq!(fs.lookup(&directory, b"..").unwrap().number(), 2);
}

fn interrupted_truncation(mode: Mode) -> Disk {
    let log = Log::new(mode, 1, 42);
    let mut fs = log.start_session();
    let reader = fs.reader().unwrap();
    let offset = reader.group(0).unwrap().inode_table() * BLOCK_SIZE as u64;
    log.disk.0.borrow_mut().fail_offset = Some(offset);
    assert_eq!(
        fs.truncate(
            PAYLOAD_INODE,
            4133,
            Timestamp::new(1_600_000_001, 0).unwrap()
        ),
        Err(Error::Io(IoError::Failed))
    );
    drop(fs);
    log.disk.crash(0);
    log.disk
}

fn store_descriptor(disk: &Disk, seed: u32, mut descriptor: Vec<u8>) {
    descriptor[30..32].fill(0);
    let seed = checksum::crc32c(seed, &0u32.to_le_bytes());
    let crc = checksum::crc32c(seed, &descriptor);
    descriptor[30..32].copy_from_slice(&(crc as u16).to_le_bytes());
    disk.set(BLOCK_SIZE as u64, &descriptor);
}

fn attribute_fixture(shared: bool) -> Disk {
    Disk::from_sparse(if shared {
        include_bytes!("fixtures/shared-attributes.sparse")
    } else {
        include_bytes!("fixtures/attributes.sparse")
    })
}

fn assert_payload_live_or_reclaimed(fs: &Ext4<Disk>) {
    let root = fs.inode(2).unwrap();
    match fs.lookup(&root, b"payload") {
        Ok(inode) => assert_eq!(inode.links(), 1),
        Err(Error::NotFound) => assert!(matches!(fs.inode(PAYLOAD_INODE), Err(Error::Corrupt))),
        Err(error) => panic!("unexpected lookup error: {error:?}"),
    }
}

fn empty_directory_fixture() -> Disk {
    prepared_fixture(|fs| {
        fs.create_directory(2, b"work", created_metadata(1))
            .unwrap();
    })
}

fn rename_fixture() -> Disk {
    prepared_fixture(|fs| {
        fs.create_file(2, b"victim", created_metadata(1)).unwrap();
    })
}

fn directory_move_fixture() -> Disk {
    prepared_fixture(|fs| {
        let (left, right) = create_parent_directories(fs);
        fs.create_directory(left.number(), b"source", created_metadata(3))
            .unwrap();
        fs.create_directory(right.number(), b"victim", created_metadata(4))
            .unwrap();
    })
}

fn symlink_fixture(target: &[u8]) -> Disk {
    prepared_fixture(|fs| {
        fs.create_symlink(2, b"link", target, created_metadata(1))
            .unwrap();
    })
}

fn prepared_fixture(prepare: impl FnOnce(&mut WritableExt4<Disk>)) -> Disk {
    let (disk, mut fs) = writable_fixture();
    prepare(&mut fs);
    fs.close().unwrap();
    disk
}

fn create_parent_directories(fs: &mut WritableExt4<Disk>) -> (Inode, Inode) {
    let left = fs
        .create_directory(2, b"left", created_metadata(1))
        .unwrap();
    let right = fs
        .create_directory(2, b"right", created_metadata(2))
        .unwrap();
    (left, right)
}

fn assert_optional_symlink(fs: &Ext4<Disk>, target: &[u8]) {
    let root = fs.inode(2).unwrap();
    match fs.lookup(&root, b"link") {
        Ok(inode) => assert_eq!(fs.read_link(&inode).unwrap(), target),
        Err(Error::NotFound) => assert!(matches!(fs.inode(14), Err(Error::Corrupt))),
        Err(error) => panic!("unexpected lookup error: {error:?}"),
    }
}

fn writable_fixture() -> (Disk, WritableExt4<Disk>) {
    let disk = Disk::fixture();
    let fs = disk.write_session();
    (disk, fs)
}

fn attribute_value(fs: &Ext4<Disk>, number: u32, name: &[u8]) -> Option<Vec<u8>> {
    let inode = fs.inode(number).unwrap();
    fs.attributes(&inode)
        .unwrap()
        .into_iter()
        .find(|attribute| attribute.index() == 1 && attribute.name() == name)
        .map(|attribute| attribute.value().to_vec())
}

fn bytes_attribute() -> Vec<u8> {
    (0..251u8).cycle().take(1004).collect()
}

fn owner_change(uid: u32, gid: u32, changed: Timestamp) -> MetadataUpdate {
    MetadataUpdate {
        uid: Some(uid),
        gid: Some(gid),
        ..MetadataUpdate::new(changed)
    }
}

fn smaller_block_fixture(block_size: u32) -> Disk {
    Disk::from_sparse(if block_size == 1024 {
        include_bytes!("fixtures/rw-1024.sparse")
    } else {
        include_bytes!("fixtures/rw-2048.sparse")
    })
}

#[path = "cases/index.rs"]
mod index;

#[path = "cases/allocation.rs"]
mod allocation;

#[path = "cases/attributes.rs"]
mod attributes;

#[path = "cases/directory.rs"]
mod directory;

#[path = "cases/journal.rs"]
mod journal;

#[path = "cases/metadata.rs"]
mod metadata;

#[path = "cases/orphan.rs"]
mod orphan;

fn assert_free_counts(fs: &WritableExt4<Disk>, expected: (u64, u32)) {
    assert_eq!(
        (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
        expected
    );
}

fn assert_attribute(fs: &WritableExt4<Disk>, number: u32, name: &[u8], expected: &[u8]) {
    assert_eq!(
        attribute_value(fs.reader().unwrap(), number, name).as_deref(),
        Some(expected)
    );
}
