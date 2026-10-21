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

    /// Computes a metadata checksum, allowing reuse while cached bytes match.
    fn metadata_crc32c(&self, _offset: u64, bytes: &[u8], seed: u32) -> u32 {
        checksum::crc32c(seed, bytes)
    }

    /// Reads filesystem metadata, allowing a backend to cache the bytes.
    fn read_metadata_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        self.read_exact_at(offset, buffer)
    }
}

/// A byte-addressed device with explicit write and flush results.
pub trait WriteAt: ReadAt {
    /// Maximum bytes of metadata that can be staged without publishing disk writes.
    fn metadata_writeback_limit(&self) -> usize {
        0
    }

    /// Makes metadata visible to readers until the journal commits it.
    fn stage_metadata(&self, _offset: u64, _buffer: &[u8]) -> Result<(), IoError> {
        Err(IoError::Unsupported)
    }

    /// Releases staged metadata after a durable journal checkpoint.
    fn clear_staged_metadata(&self) {}

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
