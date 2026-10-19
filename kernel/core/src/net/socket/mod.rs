// SPDX-License-Identifier: MPL-2.0

use core::{fmt::Display, time::Duration};

use options::{RecvTimeout, SendTimeout, SocketOption};
use util::{MessageHeader, RecvFlags, RecvOutput, SendFlags, SockShutdownCmd, SocketAddr};

use crate::{
    fs::{
        file::{
            AccessMode, CreationFlags, FileCommon, FileLike, SettableStatusFlags, StatusFlags,
            file_table::FdFlags,
        },
        pseudofs::SockFs,
    },
    net::socket::util::ioctl::socket_ioctl,
    prelude::*,
    process::PidNamespace,
    util::{MultiRead, MultiWrite, VmReaderArray, VmWriterArray, ioctl::RawIoctl},
};

pub(crate) mod ip;
pub(crate) mod netlink;
pub(crate) mod options;
pub(crate) mod unix;
pub(crate) mod util;
pub(crate) mod vsock;

mod private {
    use core::time::Duration;

    use super::{
        SocketCommon,
        util::{MessageHeader, PendingError, RecvFlags, RecvOutput},
    };
    use crate::{
        events::IoEvents, fs::file::FileCommon, prelude::*, process::signal::Pollable,
        util::ioctl::RawIoctl,
    };

    /// Common methods for sockets, but private to the network module.
    ///
    /// These are implementation details of sockets, so shouldn't be accessed outside the network
    /// module. Therefore, the whole trait is sealed.
    pub(crate) trait SocketPrivate: Pollable {
        fn socket_common(&self) -> &SocketCommon;

        fn pending_error(&self) -> &PendingError {
            &self.socket_common().error
        }

        fn common(&self) -> &FileCommon {
            &self.socket_common().file
        }

        /// Returns whether the socket is in non-blocking mode.
        fn is_nonblocking(&self) -> bool {
            self.common().is_nonblocking()
        }

        /// Blocks until some events occur to complete I/O operations.
        ///
        /// If the socket is in non-blocking mode and the I/O operations cannot be completed
        /// immediately, this method will fail with [`EAGAIN`] instead of blocking.
        ///
        /// [`EAGAIN`]: crate::error::Errno::EAGAIN
        #[track_caller]
        fn block_on<F, R>(
            &self,
            events: IoEvents,
            timeout: Option<Duration>,
            nonblocking: bool,
            mut try_op: F,
        ) -> Result<R>
        where
            Self: Sized,
            F: FnMut() -> Result<R>,
        {
            if self.is_nonblocking() || nonblocking {
                try_op()
            } else {
                self.wait_events(events, timeout.as_ref(), try_op)
                    .map_err(|err| match err.error() {
                        Errno::ETIME => {
                            Error::with_message(Errno::EAGAIN, "the socket timeout expired")
                        }
                        _ => err,
                    })
            }
        }

        fn block_on_recv<F, R>(
            &self,
            flags: RecvFlags,
            timeout: Option<Duration>,
            mut try_op: F,
        ) -> Result<R>
        where
            Self: Sized,
            F: FnMut() -> Result<R>,
        {
            self.block_on(
                IoEvents::IN,
                timeout,
                flags.contains(RecvFlags::MSG_DONTWAIT),
                || {
                    if !flags.contains(RecvFlags::MSG_ERRQUEUE) {
                        self.pending_error().check()?;
                    }
                    try_op()
                },
            )
        }

        fn wait_for_connect(
            &self,
            timeout: Option<Duration>,
            try_op: impl FnMut() -> Result<()>,
        ) -> Result<()>
        where
            Self: Sized,
        {
            self.wait_events(IoEvents::OUT, timeout.as_ref(), try_op)
                .map_err(|err| match err.error() {
                    Errno::ETIME => {
                        Error::with_message(Errno::EINPROGRESS, "the socket timeout expired")
                    }
                    _ => err,
                })
        }

        fn recv_stream(
            &self,
            flags: RecvFlags,
            timeout: Option<Duration>,
            try_op: impl FnMut() -> Result<usize>,
        ) -> Result<(RecvOutput, MessageHeader)>
        where
            Self: Sized,
        {
            flags.warn_if_unsupported();
            let count = self.block_on_recv(flags, timeout, try_op)?;
            Ok((
                RecvOutput::new_for_stream(count),
                MessageHeader::new(None, Vec::new()),
            ))
        }

        /// Handles commands specific to its protocol or socket type.
        fn protocol_ioctl(&self, _raw_ioctl: RawIoctl) -> Result<i32> {
            return_errno_with_message!(Errno::ENOTTY, "the socket ioctl command is unknown");
        }
    }
}

pub(crate) struct SocketCommon {
    file: FileCommon,
    error: Arc<util::PendingError>,
}

impl core::ops::Deref for SocketCommon {
    type Target = FileCommon;

    fn deref(&self) -> &FileCommon {
        &self.file
    }
}

/// Operations defined on a socket.
pub(crate) trait Socket: private::SocketPrivate + Send + Sync {
    fn take_error(&self) -> Option<Error> {
        self.pending_error()
            .take()
            .or_else(|| self.transport_error())
    }

    fn transport_error(&self) -> Option<Error> {
        None
    }

    fn get_error_option(&self, option: &mut dyn SocketOption) -> bool {
        let Some(option) = option.as_any_mut().downcast_mut::<options::Error>() else {
            return false;
        };
        option.set(self.take_error());
        true
    }
    /// Assigns the specified address to the socket.
    fn bind(&self, _socket_addr: SocketAddr) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "bind() is not supported");
    }

    /// Builds a connection for the given address
    fn connect(&self, _socket_addr: SocketAddr) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "connect() is not supported");
    }

    /// Listens for connections on the socket.
    fn listen(&self, _backlog: usize) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "listen() is not supported");
    }

    /// Accepts a connection on the socket.
    fn accept(&self, _is_nonblocking: bool) -> Result<(Arc<dyn FileLike>, SocketAddr)> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "accept() is not supported");
    }

    /// Shuts down part of a full-duplex connection.
    fn shutdown(&self, _cmd: SockShutdownCmd) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "shutdown() is not supported");
    }

    /// Gets the address of this socket.
    fn addr(&self) -> Result<SocketAddr> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "getsockname() is not supported");
    }

    /// Gets the address of the peer socket.
    fn peer_addr(&self) -> Result<SocketAddr> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "getpeername() is not supported");
    }

    /// Gets options on the socket.
    ///
    /// If the method succeeds, the result will be stored in the `option` parameter.
    fn get_option(&self, _option: &mut dyn SocketOption) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "getsockopt() is not supported");
    }

    /// Sets options on the socket.
    fn set_option(&self, _option: &dyn SocketOption) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "setsockopt() is not supported");
    }

    /// Returns the receive timeout; None permits signal restart.
    fn recv_timeout(&self) -> Option<Duration> {
        let mut option = RecvTimeout::new();
        self.get_option(&mut option).ok()?;
        option.get()?.duration()
    }

    /// Returns the send timeout; None permits signal restart.
    fn send_timeout(&self) -> Option<Duration> {
        let mut option = SendTimeout::new();
        self.get_option(&mut option).ok()?;
        option.get()?.duration()
    }

    /// Marks interrupted receives restartable when no receive timeout applies.
    fn restart_recv_error(&self, error: Error) -> Error {
        restart_error(error, self.recv_timeout())
    }

    /// Marks interrupted sends restartable when no send timeout applies.
    fn restart_send_error(&self, error: Error) -> Error {
        restart_error(error, self.send_timeout())
    }

    /// Sends a message on the socket.
    fn sendmsg(
        &self,
        reader: &mut dyn MultiRead,
        message_header: MessageHeader,
        flags: SendFlags,
    ) -> Result<usize>;

    /// Receives a message from the socket.
    ///
    /// If successful, the `writer` buffer will be filled with the received content.
    /// This method returns the length, flags, and header of the received message.
    fn recvmsg(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, MessageHeader)>;
}

impl<T: Socket + 'static> FileLike for T {
    fn read(&self, writer: &mut VmWriter) -> Result<usize> {
        if !writer.has_avail() {
            // Linux always returns `Ok(0)` in this case, so we follow it.
            return Ok(0);
        }

        // TODO: Set correct flags
        self.recvmsg(writer, RecvFlags::empty())
            .map(|(output, _)| output.len())
    }

    fn write(&self, reader: &mut VmReader) -> Result<usize> {
        // TODO: Set correct flags
        self.sendmsg(
            reader,
            MessageHeader::new(None, Vec::new()),
            SendFlags::empty(),
        )
    }

    fn readv(&self, writers: &mut VmWriterArray) -> Result<usize> {
        if writers.is_empty() {
            return Ok(0);
        }
        self.recvmsg(writers, RecvFlags::empty())
            .map(|(output, _)| output.len())
    }

    fn writev(&self, readers: &mut VmReaderArray) -> Result<usize> {
        self.sendmsg(
            readers,
            MessageHeader::new(None, Vec::new()),
            SendFlags::empty(),
        )
    }

    fn ioctl(&self, raw_ioctl: RawIoctl) -> Result<i32> {
        socket_ioctl(self, raw_ioctl)
    }

    fn settable_status_flags(&self) -> SettableStatusFlags {
        SettableStatusFlags::minimal().with_o_async()
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }

    fn common(&self) -> &FileCommon {
        private::SocketPrivate::common(self)
    }

    fn dump_proc_fdinfo(
        self: Arc<Self>,
        fd_flags: FdFlags,
        _namespace: &PidNamespace,
    ) -> Box<dyn Display> {
        struct FdInfo {
            flags: u32,
            ino: u64,
        }

        impl Display for FdInfo {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                writeln!(f, "pos:\t{}", 0)?;
                writeln!(f, "flags:\t0{:o}", self.flags)?;
                writeln!(f, "mnt_id:\t{}", SockFs::mount_node().id())?;
                writeln!(f, "ino:\t{}", self.ino)
            }
        }

        let mut flags = self.common().status_flags().bits() | self.common().access_mode() as u32;
        if fd_flags.contains(FdFlags::CLOEXEC) {
            flags |= CreationFlags::O_CLOEXEC.bits();
        }

        Box::new(FdInfo {
            flags,
            ino: self.common().path().inode().ino(),
        })
    }
}

/// Create the `FileCommon` for the socket files.
fn new_socket_common(is_nonblocking: bool) -> SocketCommon {
    let status_flags = if is_nonblocking {
        StatusFlags::O_NONBLOCK
    } else {
        StatusFlags::empty()
    };
    // Reference: <https://elixir.bootlin.com/linux/v7.0/source/net/socket.c#L483>.
    SocketCommon {
        file: FileCommon::new(SockFs::new_path(), AccessMode::O_RDWR, status_flags),
        error: Arc::new(util::PendingError::new()),
    }
}

fn restart_error(error: Error, timeout: Option<Duration>) -> Error {
    match error.error() {
        Errno::EINTR if timeout.is_none() => Error::new(Errno::ERESTARTSYS),
        _ => error,
    }
}
