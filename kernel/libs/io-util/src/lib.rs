// SPDX-License-Identifier: MPL-2.0

//! Low-level I/O utilities.

#![no_std]

pub mod batch;
pub mod checksum;
pub mod metadata_cache;

/// A bounded byte-addressed input device.
pub trait ReadAt {
    /// Returns the device size in bytes.
    fn len(&self) -> u64;

    /// Returns whether the device is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads the complete buffer or returns an I/O error.
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError>;

    /// Reads filesystem metadata, allowing a backend to cache the bytes.
    fn read_metadata_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        self.read_exact_at(offset, buffer)
    }
}

/// A byte-addressed device with explicit write and flush results.
pub trait WriteAt: ReadAt {
    /// Returns whether the device rejects writes.
    fn is_read_only(&self) -> bool;

    /// Writes the complete buffer or returns an I/O error.
    fn write_all_at(&self, offset: u64, buffer: &[u8]) -> Result<(), IoError>;

    /// Makes preceding writes durable, or returns an I/O error.
    fn flush(&self) -> Result<(), IoError>;
}

/// A low-level I/O error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoError {
    /// The operation is not supported by the backend.
    Unsupported,
    /// The device has no free space.
    OutOfSpace,
    /// A generic I/O failure.
    Failed,
}
