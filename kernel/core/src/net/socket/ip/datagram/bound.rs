// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::{
    errors::udp::{RecvError, SendError},
    socket::UdpReceiveMetadata,
    wire::IpEndpoint,
};

use crate::{
    events::IoEvents,
    net::{
        iface::{BoundUdpPort, Iface, UdpSocket},
        route,
        socket::{
            ip::common::get_ephemeral_endpoint,
            util::{RecvFlags, RecvOutput, SendFlags, datagram_common},
        },
    },
    prelude::*,
    util::{MultiRead, MultiWrite},
};

pub(super) struct BoundDatagram {
    bound_socket: UdpSocket,
    remote_endpoint: Option<IpEndpoint>,
}

impl BoundDatagram {
    pub(super) fn new(bound_socket: UdpSocket) -> Self {
        Self {
            bound_socket,
            remote_endpoint: None,
        }
    }

    pub(super) fn recv_packet(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, UdpReceiveMetadata)> {
        let result = self
            .bound_socket
            .recv(flags.receive_behavior(), |packet, udp_metadata| {
                let message_len = packet.len();
                let copied_res = writer
                    .write(&mut VmReader::from(packet))
                    .map_err(Into::into);
                (copied_res, udp_metadata, message_len)
            });

        match result {
            Ok((Ok(copied_len), metadata, message_len)) => {
                let output = RecvOutput::new_for_packet(flags, copied_len, message_len);
                Ok((output, metadata))
            }
            Ok((Err(e), _, _)) => Err(e),
            Err(RecvError::Exhausted) => {
                return_errno_with_message!(Errno::EAGAIN, "the receive buffer is empty")
            }
            Err(RecvError::Truncated) => {
                unreachable!("`recv` should never fail with `RecvError::Truncated`")
            }
        }
    }

    pub(super) fn iface(&self, remote: &IpEndpoint) -> Result<Arc<Iface>> {
        let first = self.bound_socket.bound_ports().next().unwrap();
        if first.addr().is_unspecified() {
            route::lookup_iface(remote.addr)
        } else {
            Ok(first.iface().clone())
        }
    }

    pub(super) fn bound_ports(&self) -> impl Iterator<Item = &BoundUdpPort> {
        self.bound_socket.bound_ports()
    }
}

impl datagram_common::Bound for BoundDatagram {
    type Endpoint = IpEndpoint;

    fn local_endpoint(&self) -> Self::Endpoint {
        self.bound_socket.local_endpoint()
    }

    fn remote_endpoint(&self) -> Option<&Self::Endpoint> {
        self.remote_endpoint.as_ref()
    }

    fn set_remote_endpoint(&mut self, endpoint: &Self::Endpoint) -> Result<()> {
        let local = self.bound_socket.local_endpoint();
        let source = if local.addr.is_unspecified() {
            get_ephemeral_endpoint(endpoint)?.addr
        } else {
            route::lookup_iface(endpoint.addr)?;
            local.addr
        };
        self.bound_socket.connect(*endpoint, source).map_err(|_| {
            Error::with_message(Errno::EAFNOSUPPORT, "the address families do not match")
        })?;
        self.remote_endpoint = Some(*endpoint);
        Ok(())
    }

    fn try_recv(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, Self::Endpoint)> {
        self.recv_packet(writer, flags)
            .map(|(output, metadata)| (output, metadata.endpoint))
    }

    fn try_send(
        &self,
        reader: &mut dyn MultiRead,
        remote: &Self::Endpoint,
        _flags: SendFlags,
    ) -> Result<usize> {
        let size = reader.sum_lens();
        if size > UdpSocket::max_payload_len(remote.addr) {
            return_errno_with_message!(Errno::EMSGSIZE, "the message is too large");
        }
        let iface = self.iface(remote)?;
        let mut payload = vec![0; size].into_boxed_slice();
        reader
            .read(&mut VmWriter::from(payload.as_mut()))
            .map_err(Error::from)?;
        let result = self.bound_socket.send(&iface, *remote, payload);

        match result {
            Ok(()) => Ok(size),
            Err(SendError::TooLarge) => {
                return_errno_with_message!(Errno::EMSGSIZE, "the message is too large");
            }
            Err(SendError::Unaddressable) => {
                return_errno_with_message!(Errno::EINVAL, "the destination address is invalid");
            }
            Err(SendError::BufferFull) => {
                return_errno_with_message!(Errno::EAGAIN, "the send buffer is full");
            }
        }
    }

    fn check_io_events(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if self.bound_socket.can_recv() {
            events |= IoEvents::IN;
        }
        if self.bound_socket.can_send() {
            events |= IoEvents::OUT;
        }
        events
    }
}
