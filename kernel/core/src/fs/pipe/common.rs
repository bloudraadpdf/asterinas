// SPDX-License-Identifier: MPL-2.0

use core::{
    num::Wrapping,
    sync::atomic::{AtomicUsize, Ordering},
};

use ostd::sync::WaitQueue;

use super::buffer::PipeBuffer;
use crate::{
    dispatch_ioctl,
    events::IoEvents,
    fs::{
        file::{AccessMode, PerOpenFileOps, SettableStatusFlags, StatusFlags},
        vfs::{inode::FileOps, path::Path},
    },
    prelude::*,
    process::{
        posix_thread::AsPosixThread,
        signal::{
            PollHandle, Pollable, Pollee,
            constants::SIGPIPE,
            signals::user::{UserSignal, UserSignalKind},
        },
    },
    util::ioctl::RawIoctl,
};

/// A handle for a pipe that implements `PerOpenFileOps`.
///
/// Once a handle for a `Pipe` exists, the corresponding pipe object will
/// not be dropped.
pub(in crate::fs) struct PipeHandle {
    inner: Arc<PipeObj>,
    owner: Arc<Mutex<PipeInner>>,
    access_mode: AccessMode,
    writer_generation: Option<usize>,
}

impl PipeHandle {
    fn new(
        inner: Arc<PipeObj>,
        owner: Arc<Mutex<PipeInner>>,
        access_mode: AccessMode,
        writer_generation: Option<usize>,
    ) -> Box<Self> {
        Box::new(Self {
            inner,
            owner,
            access_mode,
            writer_generation,
        })
    }

    fn try_read(&self, writer: &mut VmWriter) -> Result<usize> {
        // `InodeHandle` checks the access mode before calling methods in `PerOpenFileOps`.
        debug_assert!(self.access_mode.is_readable());

        self.inner.try_read(writer)
    }

    fn try_write(&self, reader: &mut VmReader, status_flags: StatusFlags) -> Result<usize> {
        // `InodeHandle` checks the access mode before calling methods in `PerOpenFileOps`.
        debug_assert!(self.access_mode.is_writable());

        self.inner.try_write(reader, status_flags)
    }

    fn bytes_to_read(&self) -> usize {
        self.inner.buffer.lock().len()
    }
}

impl Pollable for PipeHandle {
    fn poll(&self, mask: IoEvents, mut poller: Option<&mut PollHandle>) -> IoEvents {
        let mut events = IoEvents::empty();

        if self.access_mode.is_readable() {
            events |= self
                .inner
                .read_pollee
                .poll_with(mask, poller.as_deref_mut(), || self.inner.read_events());
            if self.writer_generation == Some(self.inner.writer_generation.load(Ordering::Relaxed))
            {
                events.remove(IoEvents::HUP);
            }
        }

        if self.access_mode.is_writable() {
            events |= self
                .inner
                .write_pollee
                .poll_with(mask, poller, || self.inner.write_events());
        }

        events
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        let mut owner = self.owner.lock();
        if self.access_mode.is_readable() {
            self.inner.num_reader.fetch_sub(1, Ordering::Relaxed);
        }
        if self.access_mode.is_writable() {
            self.inner.num_writer.fetch_sub(1, Ordering::Relaxed);
        }
        if self.inner.num_reader.load(Ordering::Relaxed) == 0
            && self.inner.num_writer.load(Ordering::Relaxed) == 0
        {
            owner.pipe_obj = Weak::new();
        }
        self.inner.notify_endpoints();
    }
}

impl FileOps for PipeHandle {
    fn read_at(
        &self,
        _offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if !writer.has_avail() {
            // Even the peer endpoint has been closed, reading an empty buffer is
            // still fine.
            return Ok(0);
        }

        if status_flags.contains(StatusFlags::O_NONBLOCK) {
            self.try_read(writer)
        } else {
            self.wait_events(IoEvents::IN, None, || self.try_read(writer))
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if !reader.has_remain() {
            // Even the peer endpoint has been closed, writing an empty buffer is
            // still fine.
            return Ok(0);
        }

        if status_flags.contains(StatusFlags::O_NONBLOCK) {
            self.try_write(reader, status_flags)
        } else {
            self.wait_events(IoEvents::OUT, None, || self.try_write(reader, status_flags))
        }
    }
}

impl PerOpenFileOps for PipeHandle {
    fn check_seekable(&self) -> Result<()> {
        return_errno_with_message!(Errno::ESPIPE, "the inode is a FIFO file")
    }

    fn is_offset_aware(&self) -> bool {
        false
    }

    fn ioctl(&self, _path: &Path, raw_ioctl: RawIoctl) -> Result<i32> {
        use crate::util::ioctl::common_defs::GetNumBytesToRead;

        dispatch_ioctl!(match raw_ioctl {
            cmd @ GetNumBytesToRead => {
                let bytes_to_read = self.bytes_to_read() as i32;

                cmd.write(&bytes_to_read)?;
                Ok(0)
            }
            _ => return_errno_with_message!(Errno::ENOTTY, "the ioctl command is unknown"),
        })
    }

    fn settable_status_flags(&self) -> SettableStatusFlags {
        SettableStatusFlags::minimal()
            .with_o_async()
            .with_o_direct()
    }
}

/// A pipe (FIFO) that provides inter-process communication.
///
/// Pipes are special files that appear in the filesystem and provide a
/// communication channel between processes. It can be opened multiple times
/// for reading, writing, or both.
///
/// A `Pipe` will maintain exactly one **pipe object** that provides actual pipe
/// functionalities when there is at least one handle opened on it.
pub(in crate::fs) struct Pipe {
    pipe: Arc<Mutex<PipeInner>>,
    wait_queue: WaitQueue,
}

impl Pipe {
    /// Creates a new pipe.
    pub(crate) fn new() -> Self {
        Self {
            pipe: Arc::new(Mutex::new(PipeInner::default())),
            wait_queue: WaitQueue::new(),
        }
    }

    /// Opens the named pipe with the specified access mode and status flags.
    ///
    /// Returns a handle that implements `PerOpenFileOps` for performing I/O operations.
    ///
    /// The open behavior follows POSIX semantics:
    /// - Opening for read-only blocks until a writer opens the pipe.
    /// - Opening for write-only blocks until a reader opens the pipe.
    /// - Opening for read-write never blocks.
    ///
    /// If no handle of this named pipe has existed, the method will create a new pipe object.
    /// Otherwise, it will return a handle that works on the existing pipe object.
    pub(crate) fn open_named(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Result<Box<dyn PerOpenFileOps>> {
        self.open_handle(access_mode, status_flags, true)
    }

    /// Opens the anonymous pipe with the specified access mode and status flags.
    pub(super) fn open_anon(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Result<Box<dyn PerOpenFileOps>> {
        self.open_handle(access_mode, status_flags, false)
    }

    /// Opens the pipe and returns a `PipeHandle`.
    fn open_handle(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
        is_named_pipe: bool,
    ) -> Result<Box<dyn PerOpenFileOps>> {
        let mut pipe = self.pipe.lock();
        let pipe_obj = pipe.get_or_create_pipe_obj();
        let writer_generation = (is_named_pipe
            && access_mode == AccessMode::O_RDONLY
            && status_flags.contains(StatusFlags::O_NONBLOCK)
            && pipe_obj.num_writer.load(Ordering::Relaxed) == 0)
            .then(|| pipe_obj.writer_generation.load(Ordering::Relaxed));

        let handle = match access_mode {
            AccessMode::O_RDONLY => {
                pipe.read_count += 1;

                pipe_obj.num_reader.fetch_add(1, Ordering::Relaxed);
                pipe_obj.notify_endpoints();
                self.wait_queue.wake_all();

                let has_writer = pipe_obj.num_writer.load(Ordering::Relaxed) > 0;
                let handle =
                    PipeHandle::new(pipe_obj, self.pipe.clone(), access_mode, writer_generation);

                // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/fs/pipe.c#L1166-L1175>
                if is_named_pipe && !status_flags.contains(StatusFlags::O_NONBLOCK) && !has_writer {
                    let old_write_count = pipe.write_count;
                    drop(pipe);
                    self.wait_queue.pause_until(|| {
                        (old_write_count != self.pipe.lock().write_count).then_some(())
                    })?;
                }

                handle
            }
            AccessMode::O_WRONLY => {
                pipe.write_count += 1;
                pipe_obj.writer_generation.fetch_add(1, Ordering::Relaxed);

                pipe_obj.num_writer.fetch_add(1, Ordering::Relaxed);
                pipe_obj.notify_endpoints();
                self.wait_queue.wake_all();

                let has_reader = pipe_obj.num_reader.load(Ordering::Relaxed) > 0;
                let handle =
                    PipeHandle::new(pipe_obj, self.pipe.clone(), access_mode, writer_generation);

                // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/fs/pipe.c#L1184-L1195>
                if is_named_pipe && !has_reader {
                    if status_flags.contains(StatusFlags::O_NONBLOCK) {
                        drop(pipe);
                        return_errno_with_message!(Errno::ENXIO, "no reader is present");
                    }

                    let old_read_count = pipe.read_count;
                    drop(pipe);
                    self.wait_queue.pause_until(|| {
                        (old_read_count != self.pipe.lock().read_count).then_some(())
                    })?;
                }

                handle
            }
            AccessMode::O_RDWR => {
                pipe.read_count += 1;
                pipe.write_count += 1;
                pipe_obj.writer_generation.fetch_add(1, Ordering::Relaxed);

                pipe_obj.num_reader.fetch_add(1, Ordering::Relaxed);
                pipe_obj.num_writer.fetch_add(1, Ordering::Relaxed);
                pipe_obj.notify_endpoints();
                self.wait_queue.wake_all();

                PipeHandle::new(pipe_obj, self.pipe.clone(), access_mode, writer_generation)
            }
        };

        Ok(handle)
    }
}

struct PipeObj {
    buffer: Mutex<PipeBuffer>,
    read_pollee: Pollee,
    write_pollee: Pollee,
    num_reader: AtomicUsize,
    num_writer: AtomicUsize,
    writer_generation: AtomicUsize,
}

impl PipeObj {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            buffer: Mutex::new(PipeBuffer::new()),
            read_pollee: Pollee::new(),
            write_pollee: Pollee::new(),
            num_reader: AtomicUsize::new(0),
            num_writer: AtomicUsize::new(0),
            writer_generation: AtomicUsize::new(0),
        })
    }

    fn try_read(&self, writer: &mut VmWriter) -> Result<usize> {
        let has_writers = self.num_writer.load(Ordering::Relaxed) != 0;
        let count = self.buffer.lock().read(writer)?;
        if count != 0 {
            self.read_pollee.invalidate();
            self.write_pollee.notify(IoEvents::OUT);
        } else if has_writers {
            return_errno_with_message!(Errno::EAGAIN, "the pipe is empty");
        }
        Ok(count)
    }

    fn try_write(&self, reader: &mut VmReader, status_flags: StatusFlags) -> Result<usize> {
        if self.num_reader.load(Ordering::Relaxed) == 0 {
            if let Some(posix_thread) = current_thread!().as_posix_thread() {
                posix_thread.enqueue_signal(Box::new(UserSignal::new(
                    SIGPIPE,
                    UserSignalKind::Kill,
                    posix_thread.process().pid_identity().clone(),
                    posix_thread.credentials().ruid(),
                )));
            }
            return_errno_with_message!(Errno::EPIPE, "the pipe has no readers");
        }
        let count = self.buffer.lock().write(reader, status_flags)?;
        if count == 0 {
            return_errno_with_message!(Errno::EAGAIN, "the pipe is full");
        }
        self.write_pollee.invalidate();
        self.read_pollee.notify(IoEvents::IN);
        Ok(count)
    }

    fn read_events(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if self.num_writer.load(Ordering::Relaxed) == 0 {
            events |= IoEvents::HUP;
        }
        if self.buffer.lock().len() != 0 {
            events |= IoEvents::IN;
        }
        events
    }

    fn write_events(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if self.num_reader.load(Ordering::Relaxed) == 0 {
            events |= IoEvents::ERR;
        }
        if self.buffer.lock().is_writable() {
            events |= IoEvents::OUT;
        }
        events
    }

    fn notify_endpoints(&self) {
        self.read_pollee.notify(IoEvents::IN | IoEvents::HUP);
        self.write_pollee.notify(IoEvents::OUT | IoEvents::ERR);
    }
}

#[derive(Default)]
struct PipeInner {
    pipe_obj: Weak<PipeObj>,
    read_count: Wrapping<usize>,
    write_count: Wrapping<usize>,
}

impl PipeInner {
    fn get_or_create_pipe_obj(&mut self) -> Arc<PipeObj> {
        if let Some(pipe_obj) = self.pipe_obj.upgrade() {
            return pipe_obj;
        }

        let pipe_obj = PipeObj::new();
        self.pipe_obj = Arc::downgrade(&pipe_obj);

        pipe_obj
    }
}

#[cfg(ktest)]
mod test {
    use core::sync::atomic::{self, AtomicBool};

    use ostd::prelude::*;

    use super::*;
    use crate::{
        prelude::Result,
        thread::{Thread, kernel_thread::ThreadOptions},
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Ordering {
        WriteThenRead,
        ReadThenWrite,
    }

    fn test_blocking<W, R>(write: W, read: R, ordering: Ordering)
    where
        W: FnOnce(Box<dyn PerOpenFileOps>) + Send + 'static,
        R: FnOnce(Box<dyn PerOpenFileOps>) + Send + 'static,
    {
        let pipe = Pipe::new();
        let reader = pipe
            .open_anon(AccessMode::O_RDONLY, StatusFlags::empty())
            .unwrap();
        let writer = pipe
            .open_anon(AccessMode::O_WRONLY, StatusFlags::empty())
            .unwrap();

        let signal_writer = Arc::new(AtomicBool::new(false));
        let signal_reader = signal_writer.clone();

        let writer = ThreadOptions::new(move || {
            if ordering == Ordering::ReadThenWrite {
                while !signal_writer.load(atomic::Ordering::Relaxed) {
                    Thread::yield_now();
                }
            } else {
                signal_writer.store(true, atomic::Ordering::Relaxed);
            }

            write(writer);
        })
        .spawn();

        let reader = ThreadOptions::new(move || {
            if ordering == Ordering::WriteThenRead {
                while !signal_reader.load(atomic::Ordering::Relaxed) {
                    Thread::yield_now();
                }
            } else {
                signal_reader.store(true, atomic::Ordering::Relaxed);
            }

            read(reader);
        })
        .spawn();

        writer.join();
        reader.join();
    }

    #[ktest]
    fn read_empty() {
        test_blocking(
            |writer| {
                assert_eq!(write(writer.as_ref(), &[1]).unwrap(), 1);
            },
            |reader| {
                let mut buf = [0; 1];
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 1);
                assert_eq!(&buf, &[1]);
            },
            Ordering::ReadThenWrite,
        );
    }

    #[ktest]
    fn write_full() {
        test_blocking(
            |writer| {
                assert_eq!(write(writer.as_ref(), &[1, 2, 3]).unwrap(), 2);
                assert_eq!(write(writer.as_ref(), &[2]).unwrap(), 1);
            },
            |reader| {
                let mut buf = [0; 3];
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 2);
                assert_eq!(&buf[..2], &[1, 2]);
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 1);
                assert_eq!(&buf[..1], &[2]);
            },
            Ordering::WriteThenRead,
        );
    }

    #[ktest]
    fn read_closed() {
        test_blocking(
            drop,
            |reader| {
                let mut buf = [0; 1];
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 0);
            },
            Ordering::ReadThenWrite,
        );
    }

    #[ktest]
    fn write_closed() {
        test_blocking(
            |writer| {
                assert_eq!(write(writer.as_ref(), &[1, 2, 3]).unwrap(), 2);
                assert_eq!(
                    write(writer.as_ref(), &[2]).unwrap_err().error(),
                    Errno::EPIPE
                );
            },
            drop,
            Ordering::WriteThenRead,
        );
    }

    #[ktest]
    fn write_atomicity() {
        test_blocking(
            |writer| {
                assert_eq!(write(writer.as_ref(), &[1]).unwrap(), 1);
                assert_eq!(write(writer.as_ref(), &[1, 2]).unwrap(), 2);
            },
            |reader| {
                let mut buf = [0; 3];
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 1);
                assert_eq!(&buf[..1], &[1]);
                assert_eq!(read(reader.as_ref(), &mut buf).unwrap(), 2);
                assert_eq!(&buf[..2], &[1, 2]);
            },
            Ordering::WriteThenRead,
        );
    }

    fn read(reader: &dyn PerOpenFileOps, buf: &mut [u8]) -> Result<usize> {
        reader.read_at(
            0,
            &mut VmWriter::from(buf).to_fallible(),
            StatusFlags::empty(),
        )
    }

    fn write(writer: &dyn PerOpenFileOps, buf: &[u8]) -> Result<usize> {
        writer.write_at(
            0,
            &mut VmReader::from(buf).to_fallible(),
            StatusFlags::empty(),
        )
    }
}
