// SPDX-License-Identifier: MPL-2.0

//! Device failures must remain visible through cached-file synchronisation.

use aster_block::{
    BlockDeviceMeta, SECTOR_SIZE,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::{mm::io::util::HasVmReaderWriter, prelude::ktest};

use super::*;

#[derive(Debug)]
struct FailureDisk {
    state: Mutex<DiskState>,
}

#[derive(Debug)]
struct DiskState {
    bytes: Vec<u8>,
    failure: Option<(BioType, BioStatus)>,
    failures: usize,
}

impl FailureDisk {
    fn new() -> Arc<Self> {
        let raw = include_bytes!("../../../../../libs/ext4/tests/fixtures/journal.sparse");
        assert_eq!(&raw[..8], b"EXT4SP01");
        let size = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        let mut bytes = vec![0; size];
        let mut cursor = 20;
        for _ in 0..count {
            let offset = u64::from_le_bytes(raw[cursor..cursor + 8].try_into().unwrap()) as usize;
            let length =
                u32::from_le_bytes(raw[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
            cursor += 12;
            bytes[offset..offset + length].copy_from_slice(&raw[cursor..cursor + length]);
            cursor += length;
        }
        assert_eq!(cursor, raw.len());
        Arc::new(Self {
            state: Mutex::new(DiskState {
                bytes,
                failure: None,
                failures: 0,
            }),
        })
    }
}

impl BlockDevice for FailureDisk {
    fn enqueue(&self, bio: SubmittedBio) -> core::result::Result<(), BioEnqueueError> {
        let mut state = self.state.lock();
        if let Some((kind, status)) = state.failure
            && bio.type_() == kind
        {
            state.failure = None;
            state.failures += 1;
            drop(state);
            bio.complete(status);
            return Ok(());
        }
        let mut offset = bio.sid_range().start.to_offset();
        for segment in bio.segments() {
            let end = offset + segment.nbytes();
            match bio.type_() {
                BioType::Read => {
                    segment
                        .dma_slice()
                        .writer()
                        .unwrap()
                        .write(&mut VmReader::from(&state.bytes[offset..end]));
                }
                BioType::Write => {
                    VmWriter::from(&mut state.bytes[offset..end])
                        .write(&mut segment.dma_slice().reader().unwrap());
                }
                BioType::Flush => unreachable!(),
            }
            offset = end;
        }
        drop(state);
        bio.complete(BioStatus::Complete);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            nr_sectors: self.state.lock().bytes.len() / SECTOR_SIZE,
            max_nr_segments_per_bio: usize::MAX,
            is_read_only: false,
        }
    }
    fn name(&self) -> &str {
        "ext4-failure-disk"
    }
    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(1), MinorId::new(0))
    }
}

#[ktest]
fn write_and_flush_failures_reach_vfs_sync() {
    for (kind, status) in [
        (BioType::Write, BioStatus::IoError),
        (BioType::Flush, BioStatus::IoError),
        (BioType::Flush, BioStatus::NotSupported),
        (BioType::Flush, BioStatus::NoSpace),
    ] {
        let disk = FailureDisk::new();
        let fs = create(&mut FsCreationCtx::from_block_device(
            disk.clone(),
            FsFlags::empty(),
            None,
        ))
        .unwrap();
        let root = fs.root_inode();
        let inode = root.lookup("payload").unwrap();
        let data = [0x57; PAGE_SIZE];
        inode
            .page_cache()
            .unwrap()
            .write(0, &mut VmReader::from(&data[..]).to_fallible())
            .unwrap();
        disk.state.lock().failure = Some((kind, status));
        assert_eq!(inode.sync(SyncMode::Full).unwrap_err().error(), Errno::EIO);
        assert_eq!(disk.state.lock().failures, 1);
        let mut found = [0; PAGE_SIZE];
        inode
            .page_cache()
            .unwrap()
            .read(0, &mut VmWriter::from(&mut found[..]).to_fallible())
            .unwrap();
        assert_eq!(found, data);
        drop(inode);
        drop(root);
        drop(fs);
        // Reopening through the adapter must recover the failed transaction.
        let fs = create(&mut FsCreationCtx::from_block_device(
            disk,
            FsFlags::empty(),
            None,
        ))
        .unwrap();
        let inode = fs.root_inode().lookup("payload").unwrap();
        inode
            .page_cache()
            .unwrap()
            .write(0, &mut VmReader::from(&data[..]).to_fallible())
            .unwrap();
        inode.sync(SyncMode::Full).unwrap();
        drop(inode);
        fs.sync().unwrap();
    }
}
