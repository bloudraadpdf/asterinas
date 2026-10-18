// SPDX-License-Identifier: MPL-2.0

use aster_erofs::{Erofs, Error};
use io_util::{IoError, ReadAt};

struct Image<'a>(&'a [u8]);

impl ReadAt for Image<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        let start = usize::try_from(offset).map_err(|_| IoError::Failed)?;
        let end = start.checked_add(buffer.len()).ok_or(IoError::Failed)?;
        let bytes = self.0.get(start..end).ok_or(IoError::Failed)?;
        buffer.copy_from_slice(bytes);
        Ok(())
    }
}

const IMAGES: [&[u8]; 3] = [
    include_bytes!("fixtures/flat.erofs"),
    include_bytes!("fixtures/lz4.erofs"),
    include_bytes!("fixtures/lz4-full.erofs"),
];

#[test]
fn reads_linux_generated_images() {
    for image in IMAGES {
        let mut fs = Erofs::open(Image(image)).unwrap();
        let root = fs.inode(fs.root_nid()).unwrap();
        let dir = fs.lookup(&root, b"dir").unwrap();
        let data = fs.lookup(&dir, b"data").unwrap();
        assert_eq!(data.size, 251 * 8193);
        assert_eq!(data.uid, 0);
        assert_eq!(data.gid, 0);
        assert_eq!(data.mode & 0xf000, 0x8000);
        assert_eq!(data.nid, fs.lookup(&root, b"hardlink").unwrap().nid);
        assert_eq!(data.links, 2);
        let mut bytes = vec![0; data.size as usize];
        assert_eq!(fs.read_at(&data, 0, &mut bytes).unwrap(), bytes.len());
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(*byte, (index % 251) as u8, "byte {index}");
        }
        for offset in [
            0,
            1,
            4095,
            4096,
            65534,
            65535,
            65536,
            524287,
            data.size - 1,
            data.size,
        ] {
            let mut actual = [0xa5; 137];
            let read = fs.read_at(&data, offset, &mut actual).unwrap();
            let expected = (data.size - offset).min(actual.len() as u64) as usize;
            assert_eq!(read, expected);
            assert_eq!(
                &actual[..read],
                &bytes[offset as usize..offset as usize + read]
            );
            assert!(actual[read..].iter().all(|byte| *byte == 0xa5));
        }
        let small = fs.lookup(&root, b"small").unwrap();
        let mut bytes = [0; 100];
        let count = fs.read_at(&small, 0, &mut bytes).unwrap();
        assert_eq!(&bytes[..count], b"EROFS inline\n");
        let link = fs.lookup(&root, b"symlink").unwrap();
        assert_eq!(link.mode & 0xf000, 0xa000);
        let count = fs.read_at(&link, 0, &mut bytes).unwrap();
        assert_eq!(&bytes[..count], b"dir/data");
        assert!(matches!(fs.lookup(&root, b"missing"), Err(Error::NotFound)));
        let empty = fs.lookup(&root, b"empty").unwrap();
        assert_eq!(fs.read_at(&empty, 0, &mut bytes).unwrap(), 0);
    }
}

#[test]
fn rejects_truncated_images() {
    for image in IMAGES {
        for length in [0, 1024, 1151, image.len() - 1] {
            assert!(Erofs::open(Image(&image[..length])).is_err());
        }
    }
}

#[test]
fn rejects_file_offset_overflow() {
    let mut fs = Erofs::open(Image(IMAGES[0])).unwrap();
    let root = fs.inode(fs.root_nid()).unwrap();
    let directory = fs.lookup(&root, b"dir").unwrap();
    let mut inode = fs.lookup(&directory, b"data").unwrap();
    inode.size = u64::MAX;
    for offset in [u64::MAX - 1, u64::MAX - 8192] {
        assert_eq!(fs.read_at(&inode, offset, &mut [0]), Err(Error::Corrupt));
    }
}

#[test]
fn rejects_superblock_corruption_and_unknown_features() {
    for image in IMAGES {
        for offset in [1024, 1028, 1036, 1104] {
            let mut damaged = image.to_vec();
            damaged[offset] ^= 128;
            assert!(Erofs::open(Image(&damaged)).is_err(), "byte {offset}");
        }
    }
}

#[test]
fn rejects_inode_address_overflow() {
    let fs = Erofs::open(Image(IMAGES[0])).unwrap();
    assert!(fs.inode(u64::MAX).is_err());
    assert!(fs.inode(u64::MAX / 32).is_err());
}

#[test]
fn reads_inline_attributes() {
    for image in IMAGES {
        let mut fs = Erofs::open(Image(image)).unwrap();
        let root = fs.inode(fs.root_nid()).unwrap();
        let small = fs.lookup(&root, b"small").unwrap();
        let mut names = Vec::new();
        fs.list_xattrs(&small, |name| {
            names.push(name.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(names, vec![b"user.test".to_vec()]);
        assert_eq!(fs.get_xattr(&small, b"user.test", &mut []).unwrap(), 15);
        let mut buffer = [0; 15];
        assert_eq!(fs.get_xattr(&small, b"user.test", &mut buffer).unwrap(), 15);
        assert_eq!(&buffer, b"attribute-value");
        assert_eq!(
            fs.get_xattr(&small, b"user.test", &mut [0; 14]),
            Err(Error::BufferTooSmall)
        );
        assert_eq!(
            fs.get_xattr(&small, b"user.missing", &mut []),
            Err(Error::NotFound)
        );
    }
}

#[test]
fn preserves_special_device_numbers_and_linux_allocation_units() {
    for (image, sectors) in IMAGES.into_iter().zip([4024, 24, 24]) {
        let mut fs = Erofs::open(Image(image)).unwrap();
        let root = fs.inode(fs.root_nid()).unwrap();
        let character = fs.lookup(&root, b"char").unwrap();
        assert_eq!(character.device_number(), Some((1, 3)));
        assert_eq!(fs.allocated_sectors(&character), 0);
        let block = fs.lookup(&root, b"block").unwrap();
        assert_eq!(block.device_number(), Some((259, 703710)));
        let pipe = fs.lookup(&root, b"fifo").unwrap();
        assert_eq!(pipe.mode & 0xf000, 0x1000);
        assert_eq!(pipe.device_number(), None);
        let directory = fs.lookup(&root, b"dir").unwrap();
        let data = fs.lookup(&directory, b"data").unwrap();
        assert_eq!(fs.allocated_sectors(&data), sectors);
        assert_eq!(data.device_number(), None);
    }
}
