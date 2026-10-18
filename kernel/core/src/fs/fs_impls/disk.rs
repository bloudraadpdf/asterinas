// SPDX-License-Identifier: MPL-2.0

//! Block-device filesystem registration and instance identity.

use aster_systree::SysNode;
use device_id::DeviceId;

use crate::{
    fs::vfs::{
        file_system::{FileSystem, FsFlags},
        registry::{FsCache, FsCreationCtx, FsProperties, FsType},
    },
    prelude::*,
};

pub(in crate::fs) struct DiskFsType {
    name: &'static str,
    supported_flags: FsFlags,
    create_fn: fn(&mut FsCreationCtx) -> Result<Arc<dyn FileSystem>>,
    cache: FsCache<DeviceId>,
}

impl DiskFsType {
    pub(super) const fn new(
        name: &'static str,
        supported_flags: FsFlags,
        create_fn: fn(&mut FsCreationCtx) -> Result<Arc<dyn FileSystem>>,
    ) -> Self {
        Self {
            name,
            supported_flags,
            create_fn,
            cache: FsCache::new(),
        }
    }
}

impl FsType for DiskFsType {
    type Key = DeviceId;
    fn name(&self) -> &'static str {
        self.name
    }
    fn properties(&self) -> FsProperties {
        FsProperties::NEED_DISK
    }
    fn validate(&self, ctx: &FsCreationCtx) -> Result<()> {
        if ctx.args().is_some_and(|args| !args.is_empty()) {
            return_errno!(Errno::EINVAL);
        }
        if !self.supported_flags.contains(ctx.flags()) {
            return_errno!(Errno::EOPNOTSUPP);
        }
        Ok(())
    }
    fn create(&self, ctx: &mut FsCreationCtx) -> Result<Arc<dyn FileSystem>> {
        (self.create_fn)(ctx)
    }
    fn obtain_key_and_cache(
        &self,
        ctx: &mut FsCreationCtx,
    ) -> Option<(DeviceId, &FsCache<DeviceId>)> {
        Some((ctx.resolve_block_device().ok()?.id(), &self.cache))
    }
    fn sysnode(&self) -> Option<Arc<dyn SysNode>> {
        None
    }
}
