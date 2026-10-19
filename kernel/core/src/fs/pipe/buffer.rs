// SPDX-License-Identifier: MPL-2.0

use crate::{
    fs::file::StatusFlags,
    prelude::*,
    util::{
        MultiRead, MultiWrite,
        ring_buffer::{ConsumerU8Ext, ProducerU8Ext, RbConsumer, RbProducer, RingBuffer},
    },
};

#[cfg(not(ktest))]
const PIPE_BUF: usize = 4096;
#[cfg(ktest)]
const PIPE_BUF: usize = 2;

#[cfg(not(ktest))]
const PIPE_SLOTS: usize = 16;
#[cfg(ktest)]
const PIPE_SLOTS: usize = 1;

pub(super) struct PipeBuffer {
    producer: RbProducer<u8>,
    consumer: RbConsumer<u8>,
    segments: VecDeque<Segment>,
}

struct Segment {
    offset: usize,
    len: usize,
    is_packet: bool,
}

impl PipeBuffer {
    pub(super) fn new() -> Self {
        let (producer, consumer) = RingBuffer::new(PIPE_BUF * PIPE_SLOTS).split();
        Self {
            producer,
            consumer,
            segments: VecDeque::with_capacity(PIPE_SLOTS),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.consumer.len()
    }

    pub(super) fn is_writable(&self) -> bool {
        self.segments.len() < PIPE_SLOTS
    }

    pub(super) fn read(&mut self, writer: &mut dyn MultiWrite) -> Result<usize> {
        let mut total = 0;
        while !writer.is_empty() {
            let Some(segment) = self.segments.front_mut() else {
                break;
            };
            let count = match self
                .consumer
                .read_fallible_with_max_len(writer, segment.len)
            {
                Ok(count) => count,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            segment.offset += count;
            segment.len -= count;
            total += count;
            if segment.is_packet {
                self.consumer.commit_read(segment.len);
                self.segments.pop_front();
                break;
            }
            if segment.len == 0 {
                self.segments.pop_front();
            }
            if count == 0 {
                break;
            }
        }
        Ok(total)
    }

    pub(super) fn write(
        &mut self,
        reader: &mut dyn MultiRead,
        flags: StatusFlags,
    ) -> Result<usize> {
        let mut total = 0;
        let remainder = reader.sum_lens() % PIPE_BUF;
        if let Some(tail) = self.segments.back_mut()
            && !tail.is_packet
            && remainder != 0
            && tail.offset + tail.len + remainder <= PIPE_BUF
        {
            let count = self
                .producer
                .write_fallible_with_max_len(reader, remainder)?;
            tail.len += count;
            total += count;
        }

        while !reader.is_empty() && self.is_writable() {
            let count = match self.producer.write_fallible_with_max_len(reader, PIPE_BUF) {
                Ok(count) => count,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if count == 0 {
                break;
            }
            self.segments.push_back(Segment {
                offset: 0,
                len: count,
                is_packet: flags.contains(StatusFlags::O_DIRECT),
            });
            total += count;
        }
        Ok(total)
    }
}
