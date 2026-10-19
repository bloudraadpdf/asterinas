// SPDX-License-Identifier: MPL-2.0

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use aster_rights::ReadDupOp;

use super::message::{MessageQueue, MessageReceiver};
use crate::{
    events::IoEvents,
    net::socket::{
        Socket, SocketCommon, new_socket_common,
        options::{PeerCred, SocketOption, macros::sock_option_mut},
        private::SocketPrivate,
        unix::{CUserCred, UnixSocketAddr, cred::SocketCred, ctrl_msg::AuxiliaryData},
        util::{
            MessageHeader, RecvFlags, RecvOutput, SendFlags, SockShutdownCmd, SocketAddr,
            options::{
                GetSocketLevelOption, SetSocketLevelOption, SocketOptionSet, SocketTimeouts,
            },
        },
    },
    prelude::*,
    process::signal::{PollHandle, Pollable},
    util::{MultiRead, MultiWrite, net::SockType},
};

pub(crate) struct UnixDatagramSocket {
    local_receiver: MessageReceiver,
    remote_queue: RwLock<Option<Arc<MessageQueue>>>,
    options: RwLock<OptionSet>,
    timeouts: SocketTimeouts,
    // Since datagram sockets are not connection-oriented, they typically lack well-defined peer
    // credentials. According to the Linux implementation, however, peer credentials are recorded
    // when a socket pair is created using the `socketpair` system call.
    peer_cred: Option<SocketCred>,

    is_write_shutdown: AtomicBool,
    common: SocketCommon,
}

#[derive(Clone, Debug)]
struct OptionSet {
    socket: SocketOptionSet,
}

impl OptionSet {
    pub(self) fn new() -> Self {
        Self {
            socket: SocketOptionSet::new_unix_datagram(),
        }
    }
}

impl UnixDatagramSocket {
    pub(crate) fn new(is_nonblocking: bool) -> Arc<Self> {
        Arc::new(Self::new_raw(is_nonblocking))
    }

    pub(crate) fn new_pair(is_nonblocking: bool) -> (Arc<Self>, Arc<Self>) {
        let mut socket_a = Self::new_raw(is_nonblocking);
        let mut socket_b = Self::new_raw(is_nonblocking);

        let cred = SocketCred::<ReadDupOp>::new_current();
        socket_a.peer_cred = Some(cred.dup().restrict());
        socket_b.peer_cred = Some(cred.restrict());

        let remote_queue_a = socket_a.remote_queue.get_mut();
        let remote_queue_b = socket_b.remote_queue.get_mut();

        socket_a
            .local_receiver
            .set_peer(Some(socket_b.local_receiver.queue()));
        socket_b
            .local_receiver
            .set_peer(Some(socket_a.local_receiver.queue()));
        *remote_queue_a = Some(socket_b.local_receiver.queue().clone());
        *remote_queue_b = Some(socket_a.local_receiver.queue().clone());

        (Arc::new(socket_a), Arc::new(socket_b))
    }

    fn new_raw(is_nonblocking: bool) -> Self {
        let common = new_socket_common(is_nonblocking);
        Self {
            local_receiver: MessageReceiver::new(common.error.clone()),
            remote_queue: RwLock::new(None),
            options: RwLock::new(OptionSet::new()),
            timeouts: SocketTimeouts::new(),
            peer_cred: None,
            is_write_shutdown: AtomicBool::new(false),
            common,
        }
    }

    fn do_send(
        &self,
        reader: &mut dyn MultiRead,
        mut aux_data: AuxiliaryData,
        remote: Option<UnixSocketAddr>,
        flags: SendFlags,
        timeout: Option<Duration>,
    ) -> Result<usize> {
        if self.is_write_shutdown.load(Ordering::Relaxed) {
            return_errno_with_message!(Errno::EPIPE, "the socket is shut down for writing");
        }

        let queue = if let Some(remote_addr) = remote.as_ref() {
            let connected_addr = remote_addr.connect()?;
            MessageQueue::lookup_bound(&connected_addr)?
        } else {
            let remote_queue = self.remote_queue.read();
            remote_queue.clone().ok_or_else(|| {
                Error::with_message(Errno::ENOTCONN, "the socket is not connected")
            })?
        };

        let res = queue.send(
            reader,
            &mut aux_data,
            &self.local_receiver,
            self.is_nonblocking() || flags.contains(SendFlags::MSG_DONTWAIT),
            timeout,
        );

        // A connected socket will automatically be disconnected if the remote has been closed.
        if remote.is_none() && res.is_err_and(|err| err.error() == Errno::ECONNREFUSED) {
            let mut remote_queue = self.remote_queue.write();
            // Check to ensure that we are still connected to the same remote.
            if remote_queue
                .as_ref()
                .is_some_and(|remote| Arc::ptr_eq(remote, &queue))
            {
                *remote_queue = None;
                self.local_receiver.set_peer(None);
            }
        }

        res
    }

    fn check_io_events(&self) -> IoEvents {
        let mut io_events = self.local_receiver.check_io_events();

        if self.is_write_shutdown.load(Ordering::Relaxed) && io_events.contains(IoEvents::RDHUP) {
            io_events |= IoEvents::HUP;
        }

        io_events
    }
}

impl Pollable for UnixDatagramSocket {
    fn poll(&self, mask: IoEvents, mut poller: Option<&mut PollHandle>) -> IoEvents {
        let pending = self
            .local_receiver
            .pending_error()
            .poll(poller.as_deref_mut());
        let input = self.local_receiver.pollee().poll_with(
            mask & !IoEvents::OUT,
            poller.as_deref_mut(),
            || self.check_io_events() & !IoEvents::OUT,
        );
        let mut output = self
            .local_receiver
            .send_buffer()
            .poll(mask, poller.as_deref_mut());
        let remote = self.remote_queue.read().clone();
        if let Some(queue) = remote {
            output &= queue.poll_send_space(&self.local_receiver, mask, poller);
        }
        let shutdown = if self.is_write_shutdown.load(Ordering::Relaxed) {
            mask & IoEvents::OUT
        } else {
            IoEvents::empty()
        };
        pending | input | output | shutdown
    }
}

impl SocketPrivate for UnixDatagramSocket {
    fn socket_common(&self) -> &SocketCommon {
        &self.common
    }
}

impl Socket for UnixDatagramSocket {
    fn bind(&self, socket_addr: SocketAddr) -> Result<()> {
        let addr = UnixSocketAddr::try_from(socket_addr)?;
        self.local_receiver.bind(addr)
    }

    fn connect(&self, socket_addr: SocketAddr) -> Result<()> {
        let remote_addr = UnixSocketAddr::try_from(socket_addr)?;

        let connected_addr = remote_addr.connect()?;
        let queue = MessageQueue::lookup_bound(&connected_addr)?;

        let mut remote_queue = self.remote_queue.write();
        self.local_receiver.set_peer(Some(&queue));
        *remote_queue = Some(queue);

        Ok(())
    }

    fn shutdown(&self, cmd: SockShutdownCmd) -> Result<()> {
        let mut io_events = IoEvents::empty();

        if cmd.shut_read() {
            self.local_receiver.shutdown();
            io_events |= IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP;
        }

        if cmd.shut_write() {
            self.is_write_shutdown.store(true, Ordering::Relaxed);
            io_events |= IoEvents::HUP;
        }

        self.local_receiver.pollee().notify(io_events);

        Ok(())
    }

    fn addr(&self) -> Result<SocketAddr> {
        Ok(self.local_receiver.addr().into())
    }

    fn peer_addr(&self) -> Result<SocketAddr> {
        let remote_queue = self.remote_queue.read();
        match remote_queue.as_ref() {
            Some(queue) => Ok(queue.addr().into()),
            None => return_errno_with_message!(Errno::ENOTCONN, "the socket is not connected"),
        }
    }

    fn get_option(&self, option: &mut dyn SocketOption) -> Result<()> {
        if self.get_error_option(option) {
            return Ok(());
        }

        // Deal with UNIX-socket-specific socket-level options
        match do_unix_getsockopt(option, self) {
            Err(err) if err.error() == Errno::ENOPROTOOPT => (),
            res => return res,
        }

        let options = self.options.read();

        // Deal with socket-level options
        match options
            .socket
            .get_option(option, &(&self.local_receiver, &self.timeouts))
        {
            Err(err) if err.error() == Errno::ENOPROTOOPT => (),
            res => return res,
        }

        // TODO: Deal with socket options from other levels
        warn!("only socket-level options are supported");

        return_errno_with_message!(Errno::ENOPROTOOPT, "the socket option to get is unknown")
    }

    fn set_option(&self, option: &dyn SocketOption) -> Result<()> {
        let mut options = self.options.write();
        if let Some((send, bytes)) = options.socket.set_unix_buffer_option(option)? {
            if send {
                self.local_receiver.send_buffer().set_limit(bytes as usize);
            }
            return Ok(());
        }

        match options
            .socket
            .set_option(option, &(&self.local_receiver, &self.timeouts))
        {
            Err(err) if err.error() == Errno::ENOPROTOOPT => {
                // TODO: Deal with socket options from other levels
                warn!("only socket-level options are supported");
                return_errno_with_message!(
                    Errno::ENOPROTOOPT,
                    "the socket option to get is unknown"
                )
            }
            res => res.map(|_need_iface_poll| ()),
        }
    }

    fn sendmsg(
        &self,
        reader: &mut dyn MultiRead,
        message_header: MessageHeader,
        flags: SendFlags,
    ) -> Result<usize> {
        if flags.contains(SendFlags::MSG_OOB) {
            return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "UNIX datagrams have no out-of-band data"
            );
        }
        // TODO: Deal with flags
        flags.warn_if_unsupported();

        let MessageHeader {
            addr,
            control_messages,
        } = message_header;

        let remote_addr = match addr {
            Some(addr) => Some(addr.try_into()?),
            None => None,
        };

        let auxiliary_data = AuxiliaryData::from_control(control_messages)?;

        self.do_send(
            reader,
            auxiliary_data,
            remote_addr,
            flags,
            self.timeouts.send_timeout(),
        )
    }

    fn recvmsg(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, MessageHeader)> {
        if flags.contains(RecvFlags::MSG_OOB) {
            return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "UNIX datagrams have no out-of-band data"
            );
        }
        // TODO: Deal with flags
        flags.warn_if_unsupported();

        let (output, control_messages, peer_addr) =
            self.block_on_recv(flags, self.timeouts.recv_timeout(), || {
                self.local_receiver.try_recv(writer, flags)
            })?;

        let message_header = MessageHeader::new(Some(peer_addr.into()), control_messages);

        Ok((output, message_header))
    }
}

fn do_unix_getsockopt(option: &mut dyn SocketOption, socket: &UnixDatagramSocket) -> Result<()> {
    sock_option_mut!(match option {
        socket_peer_cred @ PeerCred => {
            let peer_cred = socket
                .peer_cred
                .as_ref()
                .map(SocketCred::to_effective_c_cred)
                .unwrap_or_else(CUserCred::new_invalid);
            socket_peer_cred.set(peer_cred);
        }
        _ => return_errno_with_message!(
            Errno::ENOPROTOOPT,
            "the socket option to get is not UNIX-socket-specific"
        ),
    });

    Ok(())
}

impl GetSocketLevelOption for (&MessageReceiver, &SocketTimeouts) {
    fn socket_type(&self) -> SockType {
        SockType::SOCK_DGRAM
    }

    fn is_listening(&self) -> bool {
        false
    }

    fn socket_timeouts(&self) -> Option<&SocketTimeouts> {
        Some(self.1)
    }
}

impl SetSocketLevelOption for (&MessageReceiver, &SocketTimeouts) {
    fn set_pass_cred(&self, pass_cred: bool) {
        // TODO: According to the Linux man pages, "When this option is set and the socket
        // is not yet connected, a unique name in the abstract namespace will be generated
        // automatically." See <https://man7.org/linux/man-pages/man7/unix.7.html> for
        // details.

        self.0.set_pass_cred(pass_cred);
    }

    fn socket_timeouts(&self) -> Option<&SocketTimeouts> {
        Some(self.1)
    }
}
