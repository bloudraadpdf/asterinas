// SPDX-License-Identifier: MPL-2.0

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use ostd::sync::{WaitQueue, Waiter};
use spin::Once;

use crate::{
    events::IoEvents,
    net::socket::{
        unix::{
            UnixSocketAddr,
            addr::{UnixSocketAddrBound, UnixSocketAddrKey},
            ctrl_msg::AuxiliaryData,
            send_buffer::{SendBuffer, SendCharge},
        },
        util::{ControlMessage, PendingError, RecvFlags, RecvOutput},
    },
    prelude::*,
    process::signal::{PollHandle, Pollee},
    util::{MultiRead, MultiWrite},
};

pub(super) struct MessageQueue {
    addr: Once<UnixSocketAddr>,
    inner: Mutex<Option<Inner>>,
    is_pass_cred: AtomicBool,
    pollee: Pollee,
    send_wait_queue: WaitQueue,
    space_pollee: Pollee,
    pending_error: Arc<PendingError>,
}

struct Inner {
    messages: VecDeque<Message>,
    peer: Option<Weak<MessageQueue>>,
    is_shutdown: bool,
}

struct Message {
    bytes: Vec<u8>,
    aux: AuxiliaryData,
    src: UnixSocketAddr,
    _charge: SendCharge,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SendWait {
    Ready,
    Memory,
    Receiver,
}

impl MessageQueue {
    /// Looks up a message queue bound to the specific address in the global table.
    pub(super) fn lookup_bound(addr: &UnixSocketAddrKey) -> Result<Arc<MessageQueue>> {
        QUEUE_TABLE.get_queue(addr).ok_or_else(|| {
            Error::with_message(Errno::ECONNREFUSED, "the target socket does not exist")
        })
    }

    fn try_send(
        &self,
        reader: &mut dyn MultiRead,
        aux_data: &mut AuxiliaryData,
        source: &MessageReceiver,
        waiting: &mut SendWait,
    ) -> Result<usize> {
        let mut inner = self.inner.lock();
        let Some(inner) = inner.as_mut() else {
            return_errno_with_message!(Errno::ECONNREFUSED, "the target socket is closed");
        };
        if inner.is_shutdown {
            return_errno_with_message!(Errno::EPIPE, "the target socket is shut down");
        }

        let len = reader.sum_lens();
        let paired = inner
            .peer
            .as_ref()
            .is_some_and(|peer| peer.ptr_eq(&Arc::downgrade(&source.queue)));
        if inner.peer.is_some() && !paired {
            return_errno_with_message!(
                Errno::EPERM,
                "the receiver is connected to a different peer"
            );
        }
        if *waiting == SendWait::Memory && !source.send_buffer.is_writable() {
            return_errno_with_message!(Errno::EAGAIN, "waiting for send memory to become writable");
        }
        *waiting = SendWait::Ready;

        if !paired
            && !core::ptr::eq(self, Arc::as_ptr(&source.queue))
            && inner.messages.len() > MAX_DGRAM_QLEN
        {
            *waiting = SendWait::Receiver;
            return_errno_with_message!(Errno::EAGAIN, "the receiver datagram queue is full");
        }
        let charge = source
            .send_buffer
            .charge(len, aux_data.allocated_bytes())
            .inspect_err(|error| {
                if error.error() == Errno::EAGAIN {
                    *waiting = SendWait::Memory;
                }
            })?;

        let msg = {
            let mut bytes = vec![0; len];
            reader.read(&mut VmWriter::from(bytes.as_mut_slice()))?;

            let mut aux = core::mem::take(aux_data);
            if self.is_pass_cred.load(Ordering::Relaxed)
                || source.queue.is_pass_cred.load(Ordering::Relaxed)
            {
                aux.fill_cred();
            }

            let src = source.queue.addr();

            Message {
                bytes,
                aux,
                src,
                _charge: charge,
            }
        };

        inner.messages.push_back(msg);

        self.pollee.notify(IoEvents::IN);
        self.space_pollee.invalidate();

        Ok(len)
    }

    pub(super) fn addr(&self) -> UnixSocketAddr {
        self.addr.get().cloned().unwrap_or(UnixSocketAddr::Unnamed)
    }

    pub(super) fn poll_send_space(
        &self,
        source: &MessageReceiver,
        mask: IoEvents,
        poller: Option<&mut PollHandle>,
    ) -> IoEvents {
        if core::ptr::eq(self, Arc::as_ptr(&source.queue)) {
            return mask & IoEvents::OUT;
        }
        self.space_pollee
            .poll_with(mask & IoEvents::OUT, poller, || {
                let inner = self.inner.lock();
                let writable = inner.as_ref().is_none_or(|inner| {
                    inner.is_shutdown
                        || inner.peer.is_some()
                        || inner.messages.len() <= MAX_DGRAM_QLEN
                });
                if writable {
                    IoEvents::OUT
                } else {
                    IoEvents::empty()
                }
            })
    }

    pub(super) fn send(
        &self,
        reader: &mut dyn MultiRead,
        aux: &mut AuxiliaryData,
        source: &MessageReceiver,
        nonblocking: bool,
        timeout: Option<Duration>,
    ) -> Result<usize> {
        let mut waiting = SendWait::Ready;
        if nonblocking {
            return self.try_send(reader, aux, source, &mut waiting);
        }
        let (waiter, waker) = Waiter::new_pair();
        waiter
            .pause_until_or_timeout(
                || {
                    self.send_wait_queue.enqueue(waker.clone());
                    source.send_buffer.register(&waker);
                    match self.try_send(reader, aux, source, &mut waiting) {
                        Err(error) if error.error() == Errno::EAGAIN => None,
                        result => Some(result),
                    }
                },
                timeout.as_ref(),
            )
            .map_err(|error| match error.error() {
                Errno::ETIME => Error::with_message(Errno::EAGAIN, "the socket timeout expired"),
                _ => error,
            })?
    }
}

// Note that a message receiver corresponds to a live socket and maintains certain invariants. For
// instance, `queue.inner` is always `Some(_)`, and the queue is in the global table if it is bound
// (i.e., `addr` is not `None`).
pub(super) struct MessageReceiver {
    // `addr` should be dropped as soon as the socket file is closed,
    // so it must not belong to `MessageQueue`.
    addr: Mutex<Option<UnixSocketAddrBound>>,
    queue: Arc<MessageQueue>,
    send_buffer: Arc<SendBuffer>,
}

impl MessageReceiver {
    pub(super) fn new(pending_error: Arc<PendingError>) -> MessageReceiver {
        let inner = Inner {
            messages: VecDeque::new(),
            peer: None,
            is_shutdown: false,
        };

        let queue = MessageQueue {
            addr: Once::new(),
            inner: Mutex::new(Some(inner)),
            is_pass_cred: AtomicBool::new(false),
            pollee: Pollee::new(),
            send_wait_queue: WaitQueue::new(),
            space_pollee: Pollee::new(),
            pending_error,
        };

        Self {
            addr: Mutex::new(None),
            queue: Arc::new(queue),
            send_buffer: SendBuffer::new(UNIX_DATAGRAM_DEFAULT_BUF_SIZE),
        }
    }

    pub(super) fn set_peer(&self, peer: Option<&Arc<MessageQueue>>) {
        let (old_peer, messages) = {
            let mut inner = self.queue.inner.lock();
            let inner = inner.as_mut().unwrap();
            let new_peer = peer.map(Arc::downgrade);
            if inner
                .peer
                .as_ref()
                .zip(new_peer.as_ref())
                .is_some_and(|(old, new)| old.ptr_eq(new))
            {
                return;
            }
            let old_peer = core::mem::replace(&mut inner.peer, new_peer);
            let messages = old_peer
                .as_ref()
                .map(|_| core::mem::take(&mut inner.messages));
            (old_peer.and_then(|peer| peer.upgrade()), messages)
        };
        if let Some(messages) = messages
            && !messages.is_empty()
        {
            drop(messages);
            self.queue.pollee.invalidate();
            if let Some(old_peer) = old_peer {
                let inner = old_peer.inner.lock();
                if inner.as_ref().is_some_and(|inner| {
                    inner
                        .peer
                        .as_ref()
                        .is_some_and(|peer| peer.ptr_eq(&Arc::downgrade(&self.queue)))
                }) {
                    old_peer.pending_error.set(Error::new(Errno::ECONNRESET));
                }
            }
        }
        self.queue.space_pollee.invalidate();
        self.queue.send_wait_queue.wake_all();
    }

    pub(super) fn pending_error(&self) -> &PendingError {
        &self.queue.pending_error
    }

    pub(super) fn send_buffer(&self) -> &Arc<SendBuffer> {
        &self.send_buffer
    }

    pub(super) fn bind(&self, addr_to_bind: UnixSocketAddr) -> Result<()> {
        let mut addr = self.addr.lock();

        if addr.is_some() {
            return addr_to_bind.bind_unnamed();
        }

        let bound_addr = addr_to_bind.bind()?;
        QUEUE_TABLE.add_queue(bound_addr.to_key(), self.queue.clone());
        self.queue.addr.call_once(|| bound_addr.clone().into());
        *addr = Some(bound_addr);

        Ok(())
    }

    pub(super) fn try_recv(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, Vec<ControlMessage>, UnixSocketAddr)> {
        let mut inner = self.queue.inner.lock();
        let inner = inner.as_mut().unwrap();

        let Some(msg) = inner.messages.front() else {
            if !inner.is_shutdown {
                return_errno_with_message!(Errno::EAGAIN, "the receive buffer is empty");
            } else {
                return Ok((
                    RecvOutput::new_for_packet(flags, 0, 0),
                    Vec::new(),
                    UnixSocketAddr::Unnamed,
                ));
            }
        };

        let message_len = msg.bytes.len();
        let len = writer.write(&mut VmReader::from(msg.bytes.as_slice()))?;

        let is_pass_cred = self.queue.is_pass_cred.load(Ordering::Relaxed);
        let src = msg.src.clone();

        let behavior = flags.receive_behavior();
        let ctrl_msgs = if behavior.will_consume_data() {
            let mut message = inner.messages.pop_front().unwrap();

            self.queue.pollee.invalidate();
            // A writer may still fail if the free space is not enough.
            // So we have to wake up all the writers here.
            self.queue.send_wait_queue.wake_all();
            self.queue.space_pollee.notify(IoEvents::OUT);

            message.aux.generate_control(behavior, is_pass_cred)
        } else {
            let message = inner.messages.front_mut().unwrap();
            message.aux.generate_control(behavior, is_pass_cred)
        };

        let output = RecvOutput::new_for_packet(flags, len, message_len);
        Ok((output, ctrl_msgs, src))
    }

    pub(super) fn shutdown(&self) {
        let mut inner = self.queue.inner.lock();
        let inner = inner.as_mut().unwrap();

        inner.is_shutdown = true;
        self.queue.send_wait_queue.wake_all();
        self.queue.space_pollee.notify(IoEvents::OUT);

        // The caller will notify the pollee.
    }

    pub(super) fn set_pass_cred(&self, is_pass_cred: bool) {
        self.queue
            .is_pass_cred
            .store(is_pass_cred, Ordering::Relaxed);
    }

    pub(super) fn addr(&self) -> UnixSocketAddr {
        self.queue.addr()
    }

    pub(super) fn queue(&self) -> &Arc<MessageQueue> {
        &self.queue
    }

    pub(super) fn pollee(&self) -> &Pollee {
        &self.queue.pollee
    }

    pub(super) fn check_io_events(&self) -> IoEvents {
        let inner = self.queue.inner.lock();
        let inner = inner.as_ref().unwrap();

        if inner.is_shutdown {
            IoEvents::IN | IoEvents::RDHUP
        } else if !inner.messages.is_empty() {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }
}

impl Drop for MessageReceiver {
    fn drop(&mut self) {
        if let Some(addr) = self.addr.get_mut().as_mut() {
            QUEUE_TABLE.remove_queue(&addr.to_key());
        }

        let inner = self.queue.inner.lock().take();
        drop(inner);
        self.queue.send_wait_queue.wake_all();
        self.queue.space_pollee.notify(IoEvents::OUT);
    }
}

static QUEUE_TABLE: QueueTable = QueueTable::new();

struct QueueTable {
    message_queues: RwLock<BTreeMap<UnixSocketAddrKey, Arc<MessageQueue>>>,
}

impl QueueTable {
    pub(self) const fn new() -> Self {
        Self {
            message_queues: RwLock::new(BTreeMap::new()),
        }
    }

    pub(self) fn add_queue(&self, addr_key: UnixSocketAddrKey, queue: Arc<MessageQueue>) {
        let old_queue = self.message_queues.write().insert(addr_key, queue);
        debug_assert!(old_queue.is_none());
    }

    pub(self) fn get_queue(&self, addr_key: &UnixSocketAddrKey) -> Option<Arc<MessageQueue>> {
        self.message_queues.read().get(addr_key).cloned()
    }

    pub(self) fn remove_queue(&self, addr_key: &UnixSocketAddrKey) {
        let old_queue = self.message_queues.write().remove(addr_key);
        debug_assert!(old_queue.is_some());
    }
}

pub(in crate::net) const UNIX_DATAGRAM_DEFAULT_BUF_SIZE: usize = 65536;

const MAX_DGRAM_QLEN: usize = 10;
