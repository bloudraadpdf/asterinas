// SPDX-License-Identifier: MPL-2.0

use core::{
    num::Wrapping,
    ops::{Deref, Range},
};

use ostd::{Error as OstdError, mm::io::util::HasVmReaderWriter};
pub(crate) use ring_buffer::{Consumer, Producer, RbConsumer, RbProducer, RingBuffer};

use super::{MultiRead, MultiWrite};
use crate::prelude::*;

/// Extension methods for writing bytes to a [`Producer<u8, _>`].
pub(crate) trait ProducerU8Ext {
    /// Writes data from `reader` to the ring buffer.
    ///
    /// Returns the number of bytes written.
    fn write_fallible(&mut self, reader: &mut dyn MultiRead) -> Result<usize>;

    /// Writes up to `max_len` bytes from `reader` to the ring buffer.
    ///
    /// Returns the number of bytes written.
    fn write_fallible_with_max_len(
        &mut self,
        reader: &mut dyn MultiRead,
        max_len: usize,
    ) -> Result<usize>;
}

impl<R: Deref<Target = RingBuffer<u8>>> ProducerU8Ext for Producer<u8, R> {
    fn write_fallible(&mut self, reader: &mut dyn MultiRead) -> Result<usize> {
        self.write_fallible_with_max_len(reader, usize::MAX)
    }

    fn write_fallible_with_max_len(
        &mut self,
        reader: &mut dyn MultiRead,
        max_len: usize,
    ) -> Result<usize> {
        let free_len = self.free_len().min(max_len);

        let tail = self.tail();
        let offset = tail.0 & (self.capacity() - 1);

        let write_len = copy_ring_range(offset, free_len, self.capacity(), |offset, length| {
            let mut writer = self.segment().writer();
            writer.skip(offset).limit(length);
            reader.read(&mut writer)
        })?;

        self.commit_write(write_len);
        Ok(write_len)
    }
}

/// Extension methods for byte ring buffers.
pub(crate) trait RingBufferU8Ext {
    /// Picks data from an absolute ring-counter range into `writer` without consuming it.
    ///
    /// Returns the number of bytes copied.
    fn pick_fallible(
        &self,
        range: Range<Wrapping<usize>>,
        writer: &mut dyn MultiWrite,
    ) -> Result<usize>;
}

impl RingBufferU8Ext for RingBuffer<u8> {
    fn pick_fallible(
        &self,
        range: Range<Wrapping<usize>>,
        writer: &mut dyn MultiWrite,
    ) -> Result<usize> {
        let len = (range.end - range.start).0;
        let offset = range.start.0 & (self.capacity() - 1);

        copy_ring_range(offset, len, self.capacity(), |offset, length| {
            let mut reader = self.segment().reader();
            reader.skip(offset).limit(length);
            writer.write(&mut reader)
        })
    }
}

/// Extension methods for reading bytes from a [`Consumer<u8, _>`].
pub(crate) trait ConsumerU8Ext {
    /// Reads up to `max_len` bytes from the ring buffer into `writer`.
    ///
    /// Returns the number of bytes read.
    /// A fault leaves the buffer queued, including bytes copied before the fault.
    fn read_fallible_with_max_len(
        &mut self,
        writer: &mut dyn MultiWrite,
        max_len: usize,
    ) -> Result<usize>;
}

impl<R: Deref<Target = RingBuffer<u8>>> ConsumerU8Ext for Consumer<u8, R> {
    fn read_fallible_with_max_len(
        &mut self,
        writer: &mut dyn MultiWrite,
        max_len: usize,
    ) -> Result<usize> {
        let len = self.len().min(max_len).min(writer.sum_lens());
        let head = self.head();
        let read_len = self
            .ring_buffer()
            .pick_fallible(head..head + Wrapping(len), writer)?;
        if read_len != len {
            return_errno_with_message!(Errno::EFAULT, "the buffer copy was incomplete");
        }
        self.commit_read(read_len);
        Ok(read_len)
    }
}

fn copy_ring_range(
    offset: usize,
    length: usize,
    capacity: usize,
    mut copy: impl FnMut(usize, usize) -> Result<usize, (OstdError, usize)>,
) -> Result<usize> {
    let first = length.min(capacity - offset);
    let mut total = 0;
    for (offset, length) in [(offset, first), (0, length - first)] {
        if length == 0 {
            continue;
        }
        match copy(offset, length) {
            Ok(count) => {
                total += count;
                if count < length {
                    break;
                }
            }
            Err((error, count)) => {
                total += count;
                if total == 0 {
                    return Err(error.into());
                }
                break;
            }
        }
    }
    Ok(total)
}

#[cfg(ktest)]
mod test {
    use alloc::vec;

    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn ring_copy_preserves_partial_fault_progress() {
        assert_eq!(
            copy_ring_range(3, 3, 4, |offset, _| {
                if offset == 3 {
                    Ok(1)
                } else {
                    Err((OstdError::PageFault, 1))
                }
            })
            .unwrap(),
            2
        );
        assert!(copy_ring_range(0, 2, 4, |_, _| Err((OstdError::PageFault, 0))).is_err());
        assert_eq!(
            copy_ring_range(3, 3, 4, |offset, _| {
                assert_eq!(offset, 3);
                Ok(0)
            })
            .unwrap(),
            0
        );
    }

    #[ktest]
    fn rb_write_read_one_with_ext() {
        let rb = RingBuffer::<u8>::new(1);

        let (mut prod, mut cons) = rb.split();

        let input = [u8::MAX];
        assert_eq!(
            prod.write_fallible(&mut reader_from(input.as_slice()))
                .unwrap(),
            1
        );
        assert_eq!(
            prod.write_fallible(&mut reader_from(input.as_slice()))
                .unwrap(),
            0
        );
        assert_eq!(prod.len(), 1);

        let mut output = [0u8];
        assert_eq!(
            cons.read_fallible_with_max_len(&mut writer_from(output.as_mut_slice()), usize::MAX)
                .unwrap(),
            1
        );
        assert_eq!(
            cons.read_fallible_with_max_len(&mut writer_from(output.as_mut_slice()), usize::MAX)
                .unwrap(),
            0
        );
        assert_eq!(cons.free_len(), 1);

        assert_eq!(output, input);
    }

    #[ktest]
    fn rb_write_read_all_with_ext() {
        let rb = RingBuffer::<u8>::new(4 * PAGE_SIZE);
        assert_eq!(rb.capacity(), 4 * PAGE_SIZE);

        let (mut prod, mut cons) = rb.split();

        let step = 128;
        let mut input = vec![0u8; step];
        for i in (0..4 * PAGE_SIZE).step_by(step) {
            input.fill(i as _);
            let write_len = prod
                .write_fallible(&mut reader_from(input.as_slice()))
                .unwrap();
            assert_eq!(write_len, step);
        }
        assert!(cons.is_full());

        let mut output = vec![0u8; step];
        for i in (0..4 * PAGE_SIZE).step_by(step) {
            let read_len = cons
                .read_fallible_with_max_len(&mut writer_from(output.as_mut_slice()), usize::MAX)
                .unwrap();
            assert_eq!(read_len, step);
            assert_eq!(output[0], i as u8);
            assert_eq!(output[step - 1], i as u8);
        }
        assert!(prod.is_empty());
    }

    fn reader_from(buf: &[u8]) -> VmReader<'_> {
        VmReader::from(buf).to_fallible()
    }

    fn writer_from(buf: &mut [u8]) -> VmWriter<'_> {
        VmWriter::from(buf).to_fallible()
    }
}
