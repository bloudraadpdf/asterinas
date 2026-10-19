// SPDX-License-Identifier: MPL-2.0

use ostd::{mm::VmIo, sync::WaitQueue};

use super::{IpcAccess, IpcFlags, IpcKey, IpcPerm, IpcPermission};
use crate::{
    prelude::*,
    process::{PidIdentity, UserNamespace, credentials::capabilities::CapSet},
    time::clocks::RealTimeCoarseClock,
};

pub(super) const MSGMNI: usize = 32000;
const MSGMAX: usize = 8192;
const MSGMNB: usize = 16384;
const MSG_NOERROR: u32 = 0o10000;
const MSG_EXCEPT: u32 = 0o20000;

pub(crate) struct Message {
    kind: i64,
    data: Vec<u8>,
}

pub(crate) struct MessageQueue {
    inner: Mutex<QueueState>,
    changed: WaitQueue,
    owner: Arc<UserNamespace>,
}

struct QueueState {
    permission: IpcPermission,
    messages: VecDeque<Message>,
    bytes: usize,
    limit: usize,
    removed: bool,
    sender: Option<Arc<PidIdentity>>,
    receiver: Option<Arc<PidIdentity>>,
    sent: u64,
    received: u64,
    changed: u64,
}

#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct MsgidDs {
    permission: IpcPerm,
    sent: u64,
    received: u64,
    changed: u64,
    bytes: u64,
    messages: u64,
    limit: u64,
    sender: i32,
    receiver: i32,
    _unused1: u64,
    _unused2: u64,
}

fn now() -> u64 {
    RealTimeCoarseClock::get().read_time().as_secs()
}

impl Message {
    pub(crate) fn from_user(address: Vaddr, size: usize, ctx: &Context) -> Result<Self> {
        if size > MSGMAX {
            return_errno_with_message!(Errno::EINVAL, "the message exceeds MSGMAX");
        }
        let kind = ctx.user_space().read_val::<i64>(address)?;
        if kind <= 0 {
            return_errno_with_message!(Errno::EINVAL, "message types must be positive");
        }
        let mut data = Vec::new();
        data.try_reserve_exact(size)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        data.resize(size, 0);
        if size != 0 {
            let payload = address
                .checked_add(size_of::<i64>())
                .ok_or_else(|| Error::new(Errno::EFAULT))?;
            ctx.user_space().read_bytes(payload, &mut data)?;
        }
        Ok(Self { kind, data })
    }

    pub(crate) fn copy_to_user(&self, address: Vaddr, size: usize, ctx: &Context) -> Result<usize> {
        ctx.user_space().write_val(address, &self.kind)?;
        let count = size.min(self.data.len());
        if count != 0 {
            let payload = address
                .checked_add(size_of::<i64>())
                .ok_or_else(|| Error::new(Errno::EFAULT))?;
            ctx.user_space().write_bytes(payload, &self.data[..count])?;
        }
        Ok(count)
    }
}

impl MessageQueue {
    pub(super) fn new(key: IpcKey, mode: u16, owner: Arc<UserNamespace>, ctx: &Context) -> Self {
        let credentials = ctx.posix_thread.credentials();
        Self {
            inner: Mutex::new(QueueState {
                permission: IpcPermission::new(key, credentials.euid(), credentials.egid(), mode),
                messages: VecDeque::new(),
                bytes: 0,
                limit: MSGMNB,
                removed: false,
                sender: None,
                receiver: None,
                sent: 0,
                received: 0,
                changed: now(),
            }),
            changed: WaitQueue::new(),
            owner,
        }
    }

    pub(super) fn check_lookup(&self, mode: u16, ctx: &Context) -> Result<()> {
        self.inner
            .lock()
            .permission
            .check_access(IpcAccess::from_mode(mode), ctx, &self.owner)
    }

    pub(crate) fn status(&self, ctx: &Context) -> Result<MsgidDs> {
        let inner = self.inner.lock();
        inner.check(IpcAccess::READ, ctx, &self.owner)?;
        let pid = |identity: &Option<Arc<PidIdentity>>| {
            identity.as_ref().map_or(0, |pid| {
                pid.in_namespace(ctx.process.pid_namespace()) as i32
            })
        };
        Ok(MsgidDs {
            permission: inner.permission.abi(),
            sent: inner.sent,
            received: inner.received,
            changed: inner.changed,
            bytes: inner.bytes as u64,
            messages: inner.messages.len() as u64,
            limit: inner.limit as u64,
            sender: pid(&inner.sender),
            receiver: pid(&inner.receiver),
            ..MsgidDs::default()
        })
    }

    pub(crate) fn set_status(&self, input: &MsgidDs, ctx: &Context) -> Result<()> {
        let limit = usize::try_from(input.limit).map_err(|_| Error::new(Errno::EINVAL))?;
        let mut inner = self.inner.lock();
        inner.check_live()?;
        inner.permission.check_owner(ctx, &self.owner)?;
        if limit > MSGMNB {
            self.owner
                .check_capability(ctx.posix_thread, CapSet::SYS_RESOURCE)?;
        }
        inner.permission.update(&input.permission)?;
        inner.limit = limit;
        inner.changed = now();
        self.changed.wake_all();
        Ok(())
    }

    pub(super) fn remove(&self, ctx: &Context) -> Result<()> {
        let mut inner = self.inner.lock();
        inner.permission.check_owner(ctx, &self.owner)?;
        inner.removed = true;
        inner.messages.clear();
        inner.bytes = 0;
        self.changed.wake_all();
        Ok(())
    }

    pub(crate) fn send(&self, message: Message, flags: u32, ctx: &Context) -> Result<()> {
        let nonblocking = IpcFlags::from_bits_truncate(flags).contains(IpcFlags::IPC_NOWAIT);
        let mut message = Some(message);
        self.changed.pause_until(|| {
            let mut inner = self.inner.lock();
            if let Err(error) = inner.check(IpcAccess::WRITE, ctx, &self.owner) {
                return Some(Err(error));
            }
            let size = message.as_ref().unwrap().data.len();
            let total = inner.bytes.checked_add(size);
            let Some(total) =
                total.filter(|total| *total <= inner.limit && inner.messages.len() < inner.limit)
            else {
                return nonblocking.then(|| Err(Error::new(Errno::EAGAIN)));
            };
            if inner.messages.try_reserve(1).is_err() {
                return Some(Err(Error::new(Errno::ENOMEM)));
            }
            inner.messages.push_back(message.take().unwrap());
            inner.bytes = total;
            inner.sender = Some(ctx.process.pid_identity().clone());
            inner.sent = now();
            self.changed.wake_all();
            Some(Ok(()))
        })?
    }

    pub(crate) fn receive(
        &self,
        wanted: i64,
        size: usize,
        flags: u32,
        ctx: &Context,
    ) -> Result<Message> {
        let nonblocking = IpcFlags::from_bits_truncate(flags).contains(IpcFlags::IPC_NOWAIT);
        self.changed.pause_until(|| {
            let mut inner = self.inner.lock();
            if let Err(error) = inner.check(IpcAccess::READ, ctx, &self.owner) {
                return Some(Err(error));
            }
            let Some(index) = inner.select(wanted, flags) else {
                return nonblocking.then(|| Err(Error::new(Errno::ENOMSG)));
            };
            if size < inner.messages[index].data.len() && flags & MSG_NOERROR == 0 {
                return Some(Err(Error::new(Errno::E2BIG)));
            }
            let message = inner.messages.remove(index).unwrap();
            inner.bytes -= message.data.len();
            inner.receiver = Some(ctx.process.pid_identity().clone());
            inner.received = now();
            self.changed.wake_all();
            Some(Ok(message))
        })?
    }
}

impl QueueState {
    fn check_live(&self) -> Result<()> {
        if self.removed {
            return_errno_with_message!(Errno::EIDRM, "the message queue was removed");
        }
        Ok(())
    }

    fn check(&self, access: IpcAccess, ctx: &Context, owner: &UserNamespace) -> Result<()> {
        self.check_live()?;
        self.permission.check_access(access, ctx, owner)
    }

    fn select(&self, wanted: i64, flags: u32) -> Option<usize> {
        match wanted.cmp(&0) {
            core::cmp::Ordering::Equal => (!self.messages.is_empty()).then_some(0),
            core::cmp::Ordering::Greater => self.messages.iter().position(|message| {
                if flags & MSG_EXCEPT != 0 {
                    message.kind != wanted
                } else {
                    message.kind == wanted
                }
            }),
            core::cmp::Ordering::Less => self
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| message.kind as u64 <= wanted.unsigned_abs())
                .min_by_key(|(index, message)| (message.kind, *index))
                .map(|(index, _)| index),
        }
    }
}
