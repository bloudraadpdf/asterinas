// SPDX-License-Identifier: MPL-2.0

use std::{fs::OpenOptions, os::unix::fs::FileExt, path::Path};

use io_util::{IoError, ReadAt, WriteAt};

pub(super) struct Disk {
    file: std::fs::File,
    length: u64,
    writable: bool,
}

impl Disk {
    pub(super) fn open(path: impl AsRef<Path>, writable: bool) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            file,
            length,
            writable,
        })
    }
}

impl ReadAt for Disk {
    fn len(&self) -> u64 {
        self.length
    }
    fn read_exact_at(&self, offset: u64, bytes: &mut [u8]) -> Result<(), IoError> {
        self.file
            .read_exact_at(bytes, offset)
            .map_err(|_| IoError::Failed)
    }
}

impl WriteAt for Disk {
    fn is_read_only(&self) -> bool {
        !self.writable
    }
    fn write_all_at(&self, offset: u64, bytes: &[u8]) -> Result<(), IoError> {
        self.file
            .write_all_at(bytes, offset)
            .map_err(|_| IoError::Failed)
    }
    fn flush(&self) -> Result<(), IoError> {
        self.file.sync_all().map_err(|_| IoError::Failed)
    }
}
